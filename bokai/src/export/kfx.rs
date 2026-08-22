//! KFX format exporter: `KfxExporter` implements `Exporter` for Amazon's KFX.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, Seek, Write};

use crate::export::Exporter;
use crate::formats::kfx::auxiliary::{build_auxiliary_data_fragment, build_ruby_content_fragments};
use crate::formats::kfx::container::get_field;
use crate::formats::kfx::context::{ExportContext, LandmarkTarget};
use crate::formats::kfx::cover::{
    COVER_SECTION_NAME, build_cover_section, get_chapter_image_path, is_image_only_chapter,
    needs_standalone_cover, normalize_cover_path,
};
use crate::formats::kfx::fragment::{FragmentData, KfxFragment};
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::metadata::{
    MetadataCategory, MetadataContext, build_category_entries, generate_book_id,
};
use crate::formats::kfx::serialization::{
    SerializedEntity, create_entity_data, generate_container_id, serialize_annotated_ion,
    serialize_container,
};
use crate::formats::kfx::symbols::KfxSymbol;
use crate::formats::kfx::transforms::format_to_kfx_symbol;
use crate::import::ChapterId;
use crate::model::{
    AnchorTarget, Book, Chapter, GlobalNodeId, LandmarkType, NodeId, ResolvedLinks, Role,
};
use crate::util::detect_media_format;

/// KFX export configuration.
#[derive(Debug, Clone, Default)]
pub struct KfxConfig {
    // Future: compression, DRM settings, etc.
}

/// KFX format exporter.
///
/// Converts books to Amazon's KFX format for Kindle devices.
pub struct KfxExporter {
    #[allow(dead_code)]
    config: KfxConfig,
}

impl KfxExporter {
    /// Create a new KfxExporter with default configuration.
    pub fn new() -> Self {
        Self {
            config: KfxConfig::default(),
        }
    }

    /// Create a new KfxExporter with custom configuration.
    pub fn with_config(config: KfxConfig) -> Self {
        Self { config }
    }

    /// Export with coarse progress reporting; see
    /// [`crate::Book::export_with_progress`]. `on_progress` is called as
    /// `(phase_key, current, total, human_label)` as the container is built.
    pub fn export_with_progress<W: Write + Seek>(
        &self,
        book: &mut Book,
        writer: &mut W,
        on_progress: &dyn Fn(&str, usize, usize, &str),
    ) -> io::Result<()> {
        let data = build_kfx_container(book, on_progress)?;
        writer.write_all(&data)?;
        Ok(())
    }
}

impl Default for KfxExporter {
    fn default() -> Self {
        Self::new()
    }
}

impl Exporter for KfxExporter {
    fn export<W: Write + Seek>(&self, book: &mut Book, writer: &mut W) -> io::Result<()> {
        self.export_with_progress(book, writer, &|_, _, _, _| {})
    }
}

/// Build a complete KFX container from a book, in two passes: Pass 1 walks the
/// IR building the position map and interning symbols, emitting no Ion; Pass 2
/// generates Ion against those positions.
fn build_kfx_container(
    book: &mut Book,
    on_progress: &dyn Fn(&str, usize, usize, &str),
) -> io::Result<Vec<u8>> {
    // A pre-paginated image book (manga/comic) takes the fixed-layout
    // `yj_non_pdf_fixed_layout` route. See [`image_fxl_to_kfx`].
    if is_fixed_layout_image_book(book) {
        return image_fxl_to_kfx(book, on_progress);
    }

    let container_id = generate_container_id(&book.metadata().title);
    let mut ctx = ExportContext::new();

    // PASS 1: SURVEY — fill ctx.symbols, ctx.position_map, ctx.chapter_fragments.
    // No Ion generation.

    // A standalone cover section: the EPUB cover image differs from the first
    // spine chapter's image
    let asset_paths: Vec<_> = book.list_assets().to_vec();
    let cover_image = book.metadata().cover_image.clone();
    let first_chapter_id = book.spine().first().map(|e| e.id);

    let (standalone_cover_path, probe_path): (Option<String>, Option<String>) =
        match (cover_image, first_chapter_id) {
            (Some(cover_img), Some(first_id)) => {
                let normalized = normalize_cover_path(&cover_img, &asset_paths);
                book.load_chapter(first_id)
                    .ok()
                    .map(|first_chapter| {
                        let in_spine_image = get_chapter_image_path(&first_chapter);
                        let needs_standalone = needs_standalone_cover(&normalized, &first_chapter);
                        // The dimension probe reads the file that renders as the
                        // cover: the metadata cover on the standalone path, the
                        // chapter's single image on the in-spine titlepage path.
                        let probe = if needs_standalone {
                            Some(normalized.clone())
                        } else {
                            in_spine_image.or(Some(normalized.clone()))
                        };
                        let standalone = if needs_standalone {
                            Some(normalized)
                        } else {
                            None
                        };
                        (standalone, probe)
                    })
                    .unwrap_or((None, None))
            }
            _ => (None, None),
        };
    // Probe the cover image's pixel dimensions once, for both emission paths
    // (`build_cover_section` and `build_chapter_entities_grouped`) to size the
    // page_template's `fixed_width` / `fixed_height` to the image.
    if let Some(ref p) = probe_path
        && let Ok(bytes) = book.load_asset(std::path::Path::new(p))
        && let Some(dims) = crate::util::extract_image_dimensions(&bytes)
    {
        ctx.cover_dimensions = Some(dims);
    }

    // Per-page pixel boxes, for the sections built from image-only pages.
    ctx.page_viewports = book
        .spine()
        .iter()
        .filter_map(|e| Some((e.id, e.viewport?)))
        .collect();

    // If standalone cover needed, section offset starts at 1 (c0 reserved for cover)
    let section_offset = if standalone_cover_path.is_some() {
        1
    } else {
        0
    };

    // Collect spine info with appropriate offset
    // Generate clean short section names (like 'c0', 'c1', etc.)
    let spine_info: Vec<_> = book
        .spine()
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            // Use short identifiers like the reference KFX files do
            let section_name = format!("c{}", idx + section_offset);
            (entry.id, section_name)
        })
        .collect();

    // Register cover section in Pass 1 if standalone cover is needed
    // This ensures it appears in reading_orders.sections and landmarks point to it
    if standalone_cover_path.is_some() {
        ctx.register_section(COVER_SECTION_NAME);
        // Fragment ID for the cover section, keyed by landmarks
        let cover_section_id = ctx.next_fragment_id();
        ctx.cover_fragment_id = Some(cover_section_id);
        // Register Cover landmark pointing to the standalone cover section
        ctx.landmark_fragments.insert(
            LandmarkType::Cover,
            LandmarkTarget {
                fragment_id: cover_section_id,
                offset: 0,
                label: "cover-nav-unit".to_string(),
            },
        );
    }

    // 1a. Resolve links: forward/reverse maps and TOC targets
    let resolved = book.resolve_links()?;

    // 1b. Register link targets: href → target, for storyline `link_to`
    register_link_targets(book, &spine_info, &resolved, &mut ctx)?;

    // 1c. Survey chapters: fragment IDs, position map, source path → chapter
    let mut source_to_chapter: HashMap<String, ChapterId> = HashMap::new();

    let n_chapters = spine_info.len();
    for (i, (chapter_id, section_name)) in spine_info.iter().enumerate() {
        on_progress("survey", i + 1, n_chapters, "Analyzing chapters");
        // Register section name as symbol
        let _section_id = ctx.register_section(section_name);

        // Get the source path for this chapter (for TOC resolution)
        let source_path = book.source_id(*chapter_id).unwrap_or("").to_string();

        // Map source path to chapter ID for landmark resolution
        if !source_path.is_empty() {
            source_to_chapter.insert(source_path.clone(), *chapter_id);
        }

        // Load and survey chapter
        if let Ok(chapter) = book.load_chapter(*chapter_id) {
            survey_chapter(&chapter, *chapter_id, &source_path, &mut ctx);
        }
    }

    // 1d. Resolve landmarks: IR landmarks, then Cover/StartReading heuristics
    resolve_landmarks_from_ir(book, &source_to_chapter, &resolved, &mut ctx);

    // Fall back to heuristics if IR didn't provide Cover or StartReading
    let has_cover = ctx.landmark_fragments.contains_key(&LandmarkType::Cover);
    let has_srl = ctx
        .landmark_fragments
        .contains_key(&LandmarkType::StartReading);

    if !has_cover || !has_srl {
        for (chapter_id, _section_name) in &spine_info {
            if let Ok(chapter) = book.load_chapter(*chapter_id) {
                let is_cover = is_image_only_chapter(&chapter);
                let fragment_id = ctx.chapter_fragments.get(chapter_id).copied();

                if let Some(fid) = fragment_id {
                    if is_cover && !ctx.landmark_fragments.contains_key(&LandmarkType::Cover) {
                        ctx.landmark_fragments.insert(
                            LandmarkType::Cover,
                            LandmarkTarget {
                                fragment_id: fid,
                                offset: 0,
                                label: "cover-nav-unit".to_string(),
                            },
                        );
                    } else if !is_cover
                        && !ctx
                            .landmark_fragments
                            .contains_key(&LandmarkType::StartReading)
                    {
                        ctx.landmark_fragments.insert(
                            LandmarkType::StartReading,
                            LandmarkTarget {
                                fragment_id: fid,
                                offset: 0,
                                label: book.metadata().title.clone(),
                            },
                        );
                    }
                }

                // Stop once both are present
                if ctx.landmark_fragments.contains_key(&LandmarkType::Cover)
                    && ctx
                        .landmark_fragments
                        .contains_key(&LandmarkType::StartReading)
                {
                    break;
                }
            }
        }
    }

    // 1c. TOC strings are used directly in Ion output, no symbol interning needed

    // 1d. Register nav container names as symbols
    ctx.nav_container_symbols.toc = ctx.symbols.get_or_intern("toc");
    ctx.nav_container_symbols.headings = ctx.symbols.get_or_intern("headings");
    ctx.nav_container_symbols.landmarks = ctx.symbols.get_or_intern("landmarks");
    ctx.nav_container_symbols.page_list = ctx.symbols.get_or_intern("page_list");

    // 1e. Register resource paths and create short names
    // IMPORTANT: Short names must be interned during Pass 1 to ensure
    // consistent symbol IDs when they're referenced later in storylines
    let asset_paths: Vec<_> = book.list_assets().to_vec();
    for asset_path in &asset_paths {
        if is_media_asset(asset_path) {
            let href = asset_path.to_string_lossy().to_string();
            ctx.resource_registry.register(&href, &mut ctx.symbols);
            // Create and intern the short name (e.g., "e0")
            let short_name = ctx.resource_registry.get_or_create_name(&href);
            ctx.symbols.get_or_intern(&short_name);
        }
    }

    // After Pass 1 ctx.symbols is complete and ctx.position_map holds every EID.
    // Pass 2 content generation creates the anchors, and TOC anchor entity IDs
    // follow it.

    // The document-level writing mode, scanned from every chapter's IR style
    // pool ahead of Pass 2 style registration: `extract_ir_field` reads it to
    // tell an explicit `horizontal-tb` override from the spec default.
    ctx.document_writing_mode = book_writing_mode(book);
    ctx.document_direction = document_direction(book, ctx.document_writing_mode);
    // A per-style `writing_mode` override compares against the SOURCE's
    // content-derived mode, not the document mode above, which
    // `primary-writing-mode` may force. Unforced, the two coincide.
    ctx.style_writing_mode_baseline = dominant_writing_mode_from_ir(book);
    // The body font defers to the reader's choice — see `reader_font_family`.
    // Without this the content pins the device font and the Kindle's font
    // control has nothing left to override.
    ctx.reader_font_family = body_font_stack(book);
    // A device-recognized content language on every reflowable style, Amazon's
    // own shape (`zh-tw`).
    ctx.content_language =
        crate::formats::kfx::metadata::kfx_content_language(&book.metadata().language);

    // PASS 2: SYNTHESIS — ctx.position_map is populated; links resolve against it.

    let mut fragments = Vec::new();

    // Entity order matches reference KFX: content_features ($585), book_metadata
    // ($490), metadata ($258), document_data ($538), book_navigation ($389),
    // then sections ($260), storylines ($259) and content ($145) in runs.

    // 2a. Content features ($585). Its conditional entries describe resources
    // and section lengths built further down; this holds the slot and is rebuilt
    // from the finished fragments.
    let content_features_index = fragments.len();
    fragments.push(build_content_features_fragment(
        &ctx,
        ContentFacts::default(),
    ));

    // Offering the publisher's typefaces is a metadata claim, settled first
    ctx.has_publisher_fonts = has_publisher_fonts(book);

    // 2b. Book metadata fragment ($490) - contains categorised_metadata
    fragments.push(build_book_metadata_fragment(book, &container_id, &ctx));

    // 2c. Metadata fragment ($258) - contains reading_orders
    fragments.push(build_metadata_fragment(book.metadata(), &ctx));

    // document_data ($538) is built after chapters and lands at this index
    let document_data_index = fragments.len();

    // 2g. Chapter entities - collect separately for proper grouping
    // Note: This also collects styles during token generation
    let mut section_fragments = Vec::new();
    let mut storyline_fragments = Vec::new();
    let mut content_fragments = Vec::new();

    // Generate standalone cover section if needed (c0)
    // Note: cover_fragment_id was assigned in Pass 1 for landmark resolution
    if let Some(ref cover_path) = standalone_cover_path {
        let section_id = ctx
            .cover_fragment_id
            .expect("cover_fragment_id should be set in Pass 1");
        // Get the next fragment ID which will be the cover's content ID
        let cover_content_id = ctx.fragment_ids.peek();
        // Cover content ID for position_map: c0 carries section and content IDs
        ctx.cover_content_id = Some(cover_content_id);
        // The cover image's pixel dimensions, sizing the cover page_template's
        // fixed_width / fixed_height to the resource. A failed probe falls back
        // to a book-cover aspect.
        let (section, storyline) = build_cover_section(cover_path, section_id, &mut ctx);
        section_fragments.push(section);
        storyline_fragments.push(storyline);

        // The cover landmark targets the section's page-template id (== section_id),
        // NOT the content/storyline id — a real Amazon KFX's `cover_page` target.
        // `cover_content_id` is kept for the position map.
        if let Some(target) = ctx.landmark_fragments.get_mut(&LandmarkType::Cover) {
            target.fragment_id = section_id;
        }
    }

    for (i, (chapter_id, section_name)) in spine_info.iter().enumerate() {
        on_progress("chapters", i + 1, n_chapters, "Converting text");
        if let Ok(chapter) = book.load_chapter(*chapter_id) {
            // Set up chapter-start anchor before generating content
            ctx.begin_chapter_export(*chapter_id);

            let (section, storyline, content) =
                build_chapter_entities_grouped(&chapter, *chapter_id, section_name, &mut ctx);
            section_fragments.push(section);
            storyline_fragments.push(storyline);
            if let Some(c) = content {
                content_fragments.push(c);
            }

            // The image resources this section depends on, for the
            // container_entity_map dependency graph.
            for node_id in chapter.iter_dfs() {
                let Some(node) = chapter.node(node_id) else {
                    continue;
                };
                if node.role == crate::model::Role::Image
                    && let Some(src) = chapter.semantics.src(node_id)
                {
                    let short_name = ctx.resource_registry.get_or_create_name(src);
                    ctx.record_section_image_ref(section_name, &short_name);
                }
                // A picture reached through the stylesheet counts as much as one
                // in an `<img>`: an `<hr>` background ornament is a dependency.
                if let Some(src) = chapter
                    .styles
                    .get(node.style)
                    .and_then(|s| s.background_image.as_deref())
                {
                    let short_name = ctx.resource_registry.get_or_create_name(src);
                    ctx.record_section_image_ref(section_name, &short_name);
                }
            }
        }
    }

    // Landmark IDs take storyline content IDs, not section IDs
    ctx.fix_landmark_content_ids();

    // 2e. Book navigation, built after chapters: heading/anchor positions exist
    fragments.push(build_book_navigation_fragment_with_positions(book, &ctx));

    // Add chapter content in reference order: sections, then storylines, then content
    fragments.extend(section_fragments);
    fragments.extend(storyline_fragments);
    fragments.extend(content_fragments);

    // 2g. Style entities ($157) - generated AFTER chapters since styles are collected during token generation
    // This includes the default style plus any unique styles found in the content
    let style_fragments = build_style_fragments(&mut ctx);
    fragments.extend(style_fragments);

    // 2g-2. Ruby content fragments ($756) - grouped annotation tables referenced
    // by storyline style_events via ruby_name/ruby_id pairs.
    let ruby_fragments = build_ruby_content_fragments(&mut ctx);
    fragments.extend(ruby_fragments);

    // 2h. Anchor fragments - must come after sections/storylines/content/styles
    // This matches the reference KFX entity ordering
    let (anchor_frags, anchor_ids_by_fragment) = build_anchor_fragments(&mut ctx);
    fragments.extend(anchor_frags);

    // 2i. Auxiliary data fragments - mark sections as navigation targets
    // Generate one auxiliary_data entity per section
    if standalone_cover_path.is_some() {
        fragments.push(build_auxiliary_data_fragment(COVER_SECTION_NAME, &mut ctx));
    }
    for (_, section_name) in &spine_info {
        fragments.push(build_auxiliary_data_fragment(section_name, &mut ctx));
    }

    // 2j. Resource fragments: external_resource (metadata) + bcRawMedia (bytes)
    // each. Interior images take `encode_asset_for_kfx` (JXR plates, JPEG on an
    // undecodable input); the cover stays JPEG for the sleep-screen thumbnailer.
    let cover_filename = book.metadata().cover_image.as_ref().and_then(|c| {
        std::path::Path::new(c)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
    });
    // Interior plates become JXR in the book's color mode, captured ahead of the
    // loop to free the `book` borrow inside.
    let color_mode = book.image_color_mode();
    // Bundle only loadable resources: images a section references
    // (`section_resource_deps`), the metadata cover, and fonts matched to
    // @font-face in `build_font_fragments`.
    let referenced_names: HashSet<String> = ctx
        .section_resource_deps
        .values()
        .flat_map(|names| names.iter().cloned())
        .collect();
    let bundle_paths: Vec<_> = asset_paths
        .iter()
        .filter(|p| {
            if !is_media_asset(p) {
                return false;
            }
            if is_font_asset(p)
                || cover_filename.as_deref() == p.file_name().and_then(|s| s.to_str())
            {
                return true;
            }
            ctx.resource_registry
                .get_name(&p.to_string_lossy())
                .is_some_and(|n| referenced_names.contains(n))
        })
        .collect();
    let n_media = bundle_paths.len();
    for (i, asset_path) in bundle_paths.into_iter().enumerate() {
        on_progress("images", i + 1, n_media, "Encoding images");
        if let Ok(data) = book.load_asset(asset_path) {
            let href = asset_path.to_string_lossy().to_string();
            // A typeface is not a picture: it skips the image transcode and the
            // external_resource description, and lands in bcRawFont ($418).
            if is_font_asset(asset_path) {
                fragments.push(build_font_resource_fragment(&href, &data, &mut ctx));
                continue;
            }
            reject_unrasterizable_svg(&href, &data)?;
            let is_cover =
                cover_filename.as_deref() == asset_path.file_name().and_then(|s| s.to_str());
            let bundled = if is_cover {
                cover_jpeg_for_kfx(&data).unwrap_or(data)
            } else {
                encode_asset_for_kfx(&data, color_mode)
            };
            // external_resource ($164) - metadata about the resource
            fragments.push(build_external_resource_fragment(&href, &bundled, &mut ctx));
            // bcRawMedia ($417) - the actual bytes
            fragments.push(build_resource_fragment(&href, &bundled, &mut ctx));
        }
    }

    // 2j-2. Font entity fragments ($262)
    // These link font_family names to resource locations (from @font-face rules)
    let font_frags = build_font_fragments(book, &mut ctx);
    fragments.extend(font_frags);

    // 2k. Navigation maps: `position_id_map` plus one `section_position_id_map`
    // per section, Amazon's section-keyed shape. `section_names` is the
    // per-section name in `section_ids` order, cover first when standalone.
    let section_names: Vec<String> = ctx
        .cover_fragment_id
        .map(|_| COVER_SECTION_NAME.to_string())
        .into_iter()
        .chain(spine_info.iter().map(|(_, n)| n.clone()))
        .collect();
    let sec_pos = section_positions(&ctx, &section_names);
    fragments.push(build_position_map_fragment(&ctx, &anchor_ids_by_fragment));
    fragments.push(build_position_id_map_fragment(&sec_pos));
    fragments.extend(build_section_position_id_map_fragments(&sec_pos));
    fragments.push(build_location_map_fragment(&ctx));

    // With every resource and section length in hand, the real content_features
    // replaces the placeholder pushed at 2a, in place: the entity order is part
    // of the format.
    let facts = ContentFacts {
        max_section_pids: sec_pos
            .iter()
            .map(|s| s.eids.iter().map(|&(_, span)| span).sum::<i64>())
            .max()
            .unwrap_or(0),
        ..ContentFacts::from_fragments(&fragments)
    };
    fragments[content_features_index] = build_content_features_fragment(&ctx, facts);

    // 2l. Container metadata entities
    fragments.push(build_resource_path_fragment());
    fragments.push(build_container_entity_map_fragment(
        &container_id,
        &fragments,
        &ctx,
    ));

    // 2d. Document data ($538) at index 3, with every ID assigned
    fragments.insert(document_data_index, build_document_data_fragment(&ctx));

    // Build symbol table ION using context
    let local_syms = ctx.symbols.local_symbols();
    let symtab_ion = build_symbol_table_ion(local_syms);

    // Build format capabilities ION
    let format_caps_ion = build_format_capabilities_ion();

    // Serialize fragments to entities
    let entities = serialize_fragments(&fragments, ctx.symbols.local_symbols());

    // PASS 3: SERIALIZATION

    on_progress("finalize", 1, 1, "Finalizing");
    Ok(serialize_container(
        &container_id,
        &entities,
        &symtab_ion,
        &format_caps_ion,
    ))
}

// Pass 1: Survey Functions (NO ION GENERATION)

/// Survey a chapter during Pass 1: assign its fragment ID, build position-map
/// entries for every node, intern text and attribute strings, and track text
/// offsets for link resolution. No Ion generation.
fn survey_chapter(
    chapter: &Chapter,
    chapter_id: ChapterId,
    source_path: &str,
    ctx: &mut ExportContext,
) {
    // Begin surveying this chapter (with source path for TOC resolution)
    let _fragment_id = ctx.begin_chapter_survey(chapter_id, source_path);

    // Walk the IR tree
    survey_node(chapter, chapter.root(), ctx);

    // End surveying
    ctx.end_chapter_survey();
}

/// Recursively survey a node and its children.
fn survey_node(chapter: &Chapter, node_id: NodeId, ctx: &mut ExportContext) {
    let node = match chapter.node(node_id) {
        Some(n) => n,
        None => return,
    };

    // Skip root node processing but walk children
    if node.role == Role::Root {
        for child in chapter.children(node_id) {
            survey_node(chapter, child, ctx);
        }
        return;
    }

    // Record position for this node (for link targets)
    ctx.record_position(node_id);

    // Pass 2 records heading positions in tokens_to_ion(), where content
    // fragment IDs exist, and creates anchor entities from ResolvedLinks'
    // GlobalNodeId targets.

    // Register resources (src attributes) - creates short names like "e0"
    // Note: href and alt are used as string values, not symbols
    if let Some(src) = chapter.semantics.src(node_id) {
        ctx.resource_registry.register(src, &mut ctx.symbols);
    }

    // Track text content and advance offset
    if !node.text.is_empty() {
        let text = chapter.text(node.text);
        ctx.advance_text_offset(text.len());
        // Plain text content needs no interning
    }

    // Recurse into children
    for child in chapter.children(node_id) {
        survey_node(chapter, child, ctx);
    }
}

/// Register link targets from ResolvedLinks with the AnchorRegistry, mapping
/// each href to its resolved target (GlobalNodeId, ChapterId, or external URL).
fn register_link_targets(
    book: &mut Book,
    spine_info: &[(ChapterId, String)],
    resolved: &ResolvedLinks,
    ctx: &mut ExportContext,
) -> io::Result<()> {
    for (chapter_id, _) in spine_info {
        let chapter = book.load_chapter(*chapter_id)?;
        register_chapter_link_targets(&chapter, *chapter_id, resolved, ctx);
    }
    Ok(())
}

/// Register link targets for a single chapter.
fn register_chapter_link_targets(
    chapter: &Chapter,
    chapter_id: ChapterId,
    resolved: &ResolvedLinks,
    ctx: &mut ExportContext,
) {
    for node_id in chapter.iter_dfs() {
        let Some(node) = chapter.node(node_id) else {
            continue;
        };

        // Only process Link nodes
        if node.role != Role::Link {
            continue;
        }

        // Get the href attribute
        let Some(href) = chapter.semantics.href(node_id) else {
            continue;
        };

        let source = GlobalNodeId::new(chapter_id, node_id);

        // Look up the resolved target and register it
        if let Some(target) = resolved.get(source) {
            match target {
                AnchorTarget::Internal(target_node) => {
                    // A body-level id (promoted to NodeId::ROOT by
                    // html::transform) anchors to no element; it registers as a
                    // chapter-level target, landing on the first content fragment.
                    if target_node.node == crate::model::NodeId::ROOT {
                        ctx.anchor_registry
                            .register_chapter_target(target_node.chapter, href);
                    } else {
                        ctx.anchor_registry
                            .register_internal_target(*target_node, href);
                    }
                }
                AnchorTarget::Chapter(target_chapter) => {
                    ctx.anchor_registry
                        .register_chapter_target(*target_chapter, href);
                }
                AnchorTarget::External(url) => {
                    ctx.anchor_registry.register_external(url);
                }
            }
        }
    }
}

/// Build style fragments from the registry, the default included. Every KFX
/// storyline element carries a style reference.
fn build_style_fragments(ctx: &mut ExportContext) -> Vec<KfxFragment> {
    // `ctx.document_writing_mode` is set ahead of Pass 2 by
    // `dominant_writing_mode_from_ir`; the ingest pipeline compares each style's
    // `writing_mode` against it.

    // Normalise per-paragraph line-height to `lh` ratios over a 1.2 em baseline
    ctx.style_registry.normalize_line_heights_to_lh();

    // Drain the registry into Ion fragments, stamping the book's content
    // language on each. Cloned first to free the second `ctx` borrow.
    let lang = ctx.content_language.clone();
    let style_pairs = ctx.style_registry.drain_to_ion(&lang);

    style_pairs
        .into_iter()
        .map(|(name, ion)| KfxFragment::new(KfxSymbol::Style, &name, ion))
        .collect()
}

/// Build the metadata fragment ($258) - contains reading_orders.
fn build_metadata_fragment(meta: &crate::model::Metadata, ctx: &ExportContext) -> KfxFragment {
    // Build section list from context's registered sections
    let sections: Vec<IonValue> = ctx
        .section_ids
        .iter()
        .map(|&id| IonValue::Symbol(id))
        .collect();

    // reading_order_name should be a STRING (not a symbol) per KFX spec
    let mut order_fields = vec![
        (
            KfxSymbol::ReadingOrderName as u64,
            IonValue::Symbol(KfxSymbol::Default as u64),
        ),
        (KfxSymbol::Sections as u64, IonValue::List(sections)),
    ];

    // Page progression direction (from OPF <spine page-progression-direction="...">).
    // KFX encodes this as a symbol: $rtl (375), $ltr (376), or omitted for default.
    if let Some(ppd) = &meta.page_progression_direction {
        let dir_sym = match ppd.as_str() {
            "rtl" => Some(KfxSymbol::Rtl),
            "ltr" => Some(KfxSymbol::Ltr),
            _ => None, // "default" or unrecognised — omit field
        };
        if let Some(sym) = dir_sym {
            order_fields.push((
                KfxSymbol::PageProgressionDirection as u64,
                IonValue::Symbol(sym as u64),
            ));
        }
    }

    let reading_order = IonValue::Struct(order_fields);
    let reading_orders = IonValue::List(vec![reading_order]);

    // $258 (metadata) contains reading_orders directly
    let metadata = IonValue::Struct(vec![(KfxSymbol::ReadingOrders as u64, reading_orders)]);

    KfxFragment::singleton(KfxSymbol::Metadata, metadata)
}

/// Build the book metadata fragment ($490) — categorised_metadata, mapped from
/// IR metadata by the schema in `kfx/metadata.rs`. Also answers whether the book
/// ships a typeface of its own AND references it.
fn has_publisher_fonts(book: &mut Book) -> bool {
    if book.font_faces().is_empty() {
        return false;
    }
    book.list_assets().iter().any(|p| is_font_asset(p))
}

fn build_book_metadata_fragment(
    book: &Book,
    container_id: &str,
    ctx: &ExportContext,
) -> KfxFragment {
    use crate::formats::kfx::metadata::MetadataValue;

    let meta = book.metadata();

    // Build metadata context with transformed values
    // Cover path in metadata may not match the registered resource path exactly.
    // Try common path variations (with/without epub/ prefix, etc.)
    let cover_resource_name = meta.cover_image.as_ref().and_then(|path| {
        // Try exact path first
        if let Some(name) = ctx.resource_registry.get_name(path) {
            return Some(name);
        }
        // Try with epub/ prefix
        let with_prefix = format!("epub/{}", path);
        if let Some(name) = ctx.resource_registry.get_name(&with_prefix) {
            return Some(name);
        }
        // Try stripping leading path components to match filename
        let filename = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())?;
        // A resource ending with this filename, lexicographically smallest of
        // the matches — a deterministic pick, not HashMap order.
        ctx.resource_registry
            .iter()
            .map(|(href, _)| href)
            .filter(|href| href.ends_with(filename))
            .min()
            .and_then(|href| ctx.resource_registry.get_name(href))
    });

    // book_id: reuse `meta.identifier` with the KFX shape (23-char URL-safe
    // Base64), else derive deterministically. Stable across a round trip.
    let book_id = if !meta.identifier.is_empty() {
        if looks_like_kfx_book_id(&meta.identifier) {
            Some(meta.identifier.clone())
        } else {
            Some(generate_book_id(&meta.identifier))
        }
    } else {
        None
    };

    // ASIN from `kfx::metadata::resolve_export_asin`
    let asin = crate::formats::kfx::metadata::resolve_export_asin(meta);

    // content_id mirrors ASIN (calibre convention), the device `.sdr` state key
    let content_id = asin.clone();

    let meta_ctx = MetadataContext {
        version: Some(env!("CARGO_PKG_VERSION")),
        cover_resource_name,
        asset_id: Some(container_id),
        book_id,
        asin,
        content_id,
        has_publisher_fonts: ctx.has_publisher_fonts,
    };

    // Category order: ebook → title → audit → capability when reflowable,
    // audit → capability → title when `fixed_layout_book`.
    let mut categories: Vec<MetadataCategory> = if ctx.fixed_layout_book {
        vec![
            MetadataCategory::KindleAudit,
            MetadataCategory::KindleCapability,
            MetadataCategory::KindleTitle,
        ]
    } else {
        vec![
            MetadataCategory::KindleEbook,
            MetadataCategory::KindleTitle,
            MetadataCategory::KindleAudit,
            MetadataCategory::KindleCapability,
        ]
    };
    let fixed_layout_lock = ctx
        .fixed_layout_book
        .then_some(meta.orientation_lock)
        .flatten();
    if fixed_layout_lock.is_some() {
        categories.push(MetadataCategory::KindleEbook);
    }

    let categorised: Vec<IonValue> = categories
        .iter()
        .map(|&cat| {
            // `KindleEbook` under `fixed_layout_book` holds
            // `book_orientation_lock` alone.
            let mut entries = match (cat, fixed_layout_lock) {
                (MetadataCategory::KindleEbook, Some(lock)) => vec![(
                    "book_orientation_lock",
                    MetadataValue::Text(lock.kindle_value().to_string()),
                )],
                _ => build_category_entries(cat, meta, &meta_ctx),
            };
            if cat == MetadataCategory::KindleCapability && ctx.fixed_layout_book {
                entries.extend(fixed_layout_capabilities(ctx.double_page_spread));
            }
            let ion_entries: Vec<IonValue> = entries
                .into_iter()
                .map(|(k, v)| metadata_kv(k, &v))
                .collect();

            IonValue::Struct(vec![
                (
                    KfxSymbol::Category as u64,
                    IonValue::String(cat.as_str().to_string()),
                ),
                (KfxSymbol::Metadata as u64, IonValue::List(ion_entries)),
            ])
        })
        .collect();

    let book_metadata = IonValue::Struct(vec![(
        KfxSymbol::CategorisedMetadata as u64,
        IonValue::List(categorised),
    )]);

    KfxFragment::singleton(KfxSymbol::BookMetadata, book_metadata)
}

/// `kindle_capability_metadata` entries for a fixed-layout image book, in the
/// key order and Ion int values Amazon writes.
fn fixed_layout_capabilities(
    double_page_spread: bool,
) -> Vec<(&'static str, crate::formats::kfx::metadata::MetadataValue)> {
    use crate::formats::kfx::metadata::MetadataValue;
    let mut entries = vec![("continuous_popup_progression", MetadataValue::Int(0))];
    if double_page_spread {
        entries.push(("yj_double_page_spread", MetadataValue::Int(1)));
    }
    entries.push(("yj_fixed_layout", MetadataValue::Int(1)));
    entries
}

/// Helper to create a metadata key-value struct. `value` may be a string or
/// an Ion-native boolean (Amazon and calibre both emit `is_sample` and
/// `override_kindle_font` as bool literals).
fn metadata_kv(key: &str, value: &crate::formats::kfx::metadata::MetadataValue) -> IonValue {
    let ion_value = match value {
        crate::formats::kfx::metadata::MetadataValue::Text(s) => IonValue::String(s.clone()),
        crate::formats::kfx::metadata::MetadataValue::Bool(b) => IonValue::Bool(*b),
        crate::formats::kfx::metadata::MetadataValue::Int(n) => IonValue::Int(*n),
    };
    IonValue::Struct(vec![
        (KfxSymbol::Key as u64, IonValue::String(key.to_string())),
        (KfxSymbol::Value as u64, ion_value),
    ])
}

// KFX book_id shape: 23 chars, URL-safe Base64 — what `generate_book_id` emits.
fn looks_like_kfx_book_id(s: &str) -> bool {
    s.len() == 23
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A `content_features` feature struct `{namespace, key, version_info}`.
fn content_feature(namespace: &str, key: &str, major: i64) -> IonValue {
    IonValue::Struct(vec![
        (
            KfxSymbol::Namespace as u64,
            IonValue::String(namespace.to_string()),
        ),
        (KfxSymbol::Key as u64, IonValue::String(key.to_string())),
        (
            KfxSymbol::VersionInfo as u64,
            IonValue::Struct(vec![(
                KfxSymbol::Version as u64,
                IonValue::Struct(vec![
                    (KfxSymbol::MajorVersion as u64, IonValue::Int(major)),
                    (KfxSymbol::MinorVersion as u64, IonValue::Int(0)),
                ]),
            )]),
        ),
    ])
}

/// The content facts a `content_features` declaration has to agree with. Each
/// conditional entry is gated on the fact it asserts, read back off the finished
/// fragments. The mirror-image check lives in [`crate::validate::source::kfx`].
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
struct ContentFacts {
    /// A JPEG-XR plate — `yj_jpegxr_sd`.
    jxr_image: bool,
    /// A JPEG payload carrying restart markers — `yj_jpg_rst_marker_present`.
    jpeg_restart_markers: bool,
    /// Positions in the longest section, for `reflow-section-size`.
    max_section_pids: i64,
}

/// Positions a section may hold before large-section support is declared
const SECTION_PID_BOUND: i64 = 65536;

impl ContentFacts {
    /// Read the media facts off the built fragments: every `external_resource`
    /// ($164) for its format, every `bcRawMedia` ($417) JPEG payload for
    /// restart markers (`FF D0`–`FF D7`).
    fn from_fragments(fragments: &[KfxFragment]) -> Self {
        let mut facts = Self::default();
        for frag in fragments {
            match (frag.ftype, &frag.data) {
                (t, FragmentData::Ion(ion)) if t == KfxSymbol::ExternalResource as u64 => {
                    let format = ion
                        .as_struct()
                        .and_then(|f| get_field(f, KfxSymbol::Format as u64))
                        .and_then(|v| match v.unwrap_annotated() {
                            IonValue::Symbol(sym) => Some(*sym),
                            _ => None,
                        });
                    if format == Some(KfxSymbol::Jxr as u64) {
                        facts.jxr_image = true;
                    }
                }
                // Only JPEG payloads: `FF D0`-`FF D7` is a marker inside JPEG
                // entropy-coded data, and a font may hold those bytes for
                // unrelated reasons.
                (t, FragmentData::Raw(bytes))
                    if t == KfxSymbol::Bcrawmedia as u64
                        && !facts.jpeg_restart_markers
                        && bytes.starts_with(&[0xFF, 0xD8, 0xFF])
                        && bytes
                            .windows(2)
                            .any(|w| w[0] == 0xFF && (0xD0..=0xD7).contains(&w[1])) =>
                {
                    facts.jpeg_restart_markers = true;
                }
                _ => {}
            }
        }
        facts
    }

    /// The `reflow-section-size` for the longest section, or `None` when no
    /// section is long enough to need one.
    fn reflow_section_size(&self) -> Option<i64> {
        (self.max_section_pids > SECTION_PID_BOUND)
            .then(|| (((self.max_section_pids - SECTION_PID_BOUND) / 16384) + 2).min(256))
    }
}

/// Build the content features fragment ($585) — the book's content
/// capabilities. Every conditional entry is gated on a [`ContentFacts`] flag.
fn build_content_features_fragment(ctx: &ExportContext, facts: ContentFacts) -> KfxFragment {
    // Build feature entries matching reference KFX
    let reflow_style = IonValue::Struct(vec![
        (
            KfxSymbol::Namespace as u64,
            IonValue::String("com.amazon.yjconversion".to_string()),
        ),
        (
            KfxSymbol::Key as u64,
            IonValue::String("reflow-style".to_string()),
        ),
        (
            KfxSymbol::VersionInfo as u64,
            IonValue::Struct(vec![(
                KfxSymbol::Version as u64,
                IonValue::Struct(vec![
                    (KfxSymbol::MajorVersion as u64, IonValue::Int(6)),
                    (KfxSymbol::MinorVersion as u64, IonValue::Int(0)),
                ]),
            )]),
        ),
    ]);

    let canonical_format = IonValue::Struct(vec![
        (
            KfxSymbol::Namespace as u64,
            IonValue::String("SDK.Marker".to_string()),
        ),
        (
            KfxSymbol::Key as u64,
            IonValue::String("CanonicalFormat".to_string()),
        ),
        (
            KfxSymbol::VersionInfo as u64,
            IonValue::Struct(vec![(
                KfxSymbol::Version as u64,
                IonValue::Struct(vec![
                    (KfxSymbol::MajorVersion as u64, IonValue::Int(1)),
                    (KfxSymbol::MinorVersion as u64, IonValue::Int(0)),
                ]),
            )]),
        ),
    ]);

    let mut features = vec![reflow_style, canonical_format];

    // `yj_hdv` is never declared: it covers tiled high-definition imagery.

    // JPEG-XR plates, which the interior-image path encodes by default.
    if facts.jxr_image {
        features.push(content_feature(
            "com.amazon.yjconversion",
            "yj_jpegxr_sd",
            1,
        ));
    }

    // Restart markers let a renderer decode a JPEG in segments.
    if facts.jpeg_restart_markers {
        features.push(content_feature(
            "com.amazon.yjconversion",
            "yj_jpg_rst_marker_present",
            1,
        ));
    }

    // Sections past 65536 positions declare the renderer's large-section
    // support, scaled to the overflow; deep paging reads the declaration.
    if let Some(size) = facts.reflow_section_size() {
        features.push(content_feature(
            "com.amazon.yjconversion",
            "reflow-section-size",
            size,
        ));
    }

    // CJK reflow-language marker, stamped alongside the book `language` for the
    // device's per-script reflow typography. `content_language` is the per-style
    // form (`zh-tw`/`ja`), which the classifier maps back to the marker.
    if let Some((key, major)) =
        crate::formats::kfx::metadata::cjk_reflow_feature(&ctx.content_language)
    {
        features.push(content_feature("com.amazon.yjconversion", key, major));

        // Japanese vertical layout has a dedicated feature; Chinese vertical
        // rides the base marker plus `document_data.writing_mode`. It tracks
        // vertical runs, not the document default.
        if ctx.has_vertical_content() && key == "jp-reflow-language" {
            features.push(content_feature(
                "com.amazon.yjconversion",
                "jpvertical-reflow-language",
                6,
            ));
        }
    }

    // A horizontal document default over vertical content is the one shape
    // needing the axes announced. Inline horizontal spans (tate-chu-yoko) are
    // not mixing.
    if !ctx.is_vertical_document() && ctx.has_vertical_content() {
        features.push(content_feature(
            "com.amazon.yjconversion",
            "yj_mixed_writing_mode",
            1,
        ));
    }

    let content_features =
        IonValue::Struct(vec![(KfxSymbol::Features as u64, IonValue::List(features))]);

    KfxFragment::singleton(KfxSymbol::ContentFeatures, content_features)
}

/// Build the document data fragment ($538).
///
/// Contains document-level settings like direction, font size, line height, max_id.
fn build_document_data_fragment(ctx: &ExportContext) -> KfxFragment {
    // Build section list from context's registered sections
    let sections: Vec<IonValue> = ctx
        .section_ids
        .iter()
        .map(|&id| IonValue::Symbol(id))
        .collect();

    let reading_order = IonValue::Struct(vec![
        (
            KfxSymbol::ReadingOrderName as u64,
            IonValue::Symbol(KfxSymbol::Default as u64),
        ),
        (KfxSymbol::Sections as u64, IonValue::List(sections)),
    ]);

    // Calculate max_id from context (highest EID used)
    let max_id = ctx.max_eid();

    // Picked up by `build_style_fragments` ahead of the registry drain
    let document_writing_mode = ctx.document_writing_mode;

    let document_data = IonValue::Struct(vec![
        (
            KfxSymbol::Direction as u64,
            IonValue::Symbol(ctx.document_direction as u64),
        ),
        (
            KfxSymbol::ColumnCount as u64,
            IonValue::Symbol(KfxSymbol::Auto as u64),
        ),
        (
            KfxSymbol::FontSize as u64,
            IonValue::Struct(vec![
                (KfxSymbol::Value as u64, IonValue::Decimal("1".to_string())),
                (
                    KfxSymbol::Unit as u64,
                    IonValue::Symbol(KfxSymbol::Em as u64),
                ),
            ]),
        ),
        (
            KfxSymbol::WritingMode as u64,
            IonValue::Symbol(document_writing_mode as u64),
        ),
        (
            KfxSymbol::Selection as u64,
            IonValue::Symbol(KfxSymbol::Enabled as u64),
        ),
        (KfxSymbol::MaxId as u64, IonValue::Int(max_id as i64)),
        (
            KfxSymbol::LineHeight as u64,
            IonValue::Struct(vec![
                (
                    KfxSymbol::Value as u64,
                    IonValue::Decimal(
                        crate::formats::kfx::style_schema::DOCUMENT_LINE_HEIGHT_EM.to_string(),
                    ),
                ),
                (
                    KfxSymbol::Unit as u64,
                    IonValue::Symbol(KfxSymbol::Em as u64),
                ),
            ]),
        ),
        // No `spacing_percent_base` here. `width` pins percentage-spacing to
        // the horizontal axis, which in a vertical-rl book aims the Layout >
        // Spacing slider at page margins. Calibre-generated KFX omits it.
        (
            KfxSymbol::ReadingOrders as u64,
            IonValue::List(vec![reading_order]),
        ),
    ]);

    KfxFragment::singleton(KfxSymbol::DocumentData, document_data)
}

/// Resolve `document_data.direction` from the book's page-progression. A
/// `-rl` `writing_mode` carries the rtl turn on device and keeps `direction:
/// ltr`; a horizontal right-to-left book states `rtl` here. Everything else ltr.
fn document_direction(book: &Book, writing_mode: KfxSymbol) -> KfxSymbol {
    let turns_rtl = book
        .metadata()
        .page_progression_direction
        .as_deref()
        .map(str::trim)
        .map(|d| d.eq_ignore_ascii_case("rtl"))
        .unwrap_or(false);
    direction_for_progression(turns_rtl, writing_mode)
}

/// Pure core of [`document_direction`]: `Rtl` only when the book turns rtl and
/// its writing mode cannot signal that turn (anything but `vertical_rl`).
fn direction_for_progression(turns_rtl: bool, writing_mode: KfxSymbol) -> KfxSymbol {
    if turns_rtl && writing_mode != KfxSymbol::VerticalRl {
        KfxSymbol::Rtl
    } else {
        KfxSymbol::Ltr
    }
}

/// The book-level writing mode for `document_data.writing_mode`: the OPF
/// `<meta name="primary-writing-mode">` hint where present, else recovered from
/// the content by [`dominant_writing_mode_from_ir`].
fn book_writing_mode(book: &mut Book) -> KfxSymbol {
    // `primary-writing-mode` is Amazon's book-level hint, encoding both text
    // axis and page-turn direction — NOT a CSS `writing-mode`. Only the axis
    // maps to KFX `writing_mode`; both `horizontal-*` values collapse.
    if let Some(pwm) = book.metadata().primary_writing_mode.as_deref() {
        match pwm.trim() {
            "vertical-rl" | "vertical_rl" => return KfxSymbol::VerticalRl,
            "vertical-lr" | "vertical_lr" => return KfxSymbol::VerticalLr,
            "horizontal-lr" | "horizontal-rl" | "horizontal-tb" | "horizontal_lr"
            | "horizontal_rl" | "horizontal_tb" => return KfxSymbol::HorizontalTb,
            _ => {} // unrecognised value — fall through to content derivation
        }
    }
    dominant_writing_mode_from_ir(book)
}

/// The font family in effect at `id` — the nearest self-or-ancestor style that
/// names one, mirroring how `font-family` inherits.
fn inherited_font_family(chapter: &Chapter, id: NodeId) -> Option<&str> {
    let mut cur = Some(id);
    while let Some(node_id) = cur {
        let node = chapter.node(node_id)?;
        if let Some(family) = chapter
            .styles
            .get(node.style)
            .and_then(|s| s.font_family.as_deref())
        {
            return Some(family);
        }
        cur = node.parent;
    }
    None
}

/// The `font-family` stack carrying the book's body text — the stack handed
/// back to the reader as `default`. Chosen by the share of text it sets, with
/// the reader's own font competing; `None` where that plurality wins.
fn body_font_stack(book: &mut Book) -> Option<String> {
    let mut covered: HashMap<String, usize> = HashMap::new();
    let mut reader_governed = 0usize;
    let chapter_ids: Vec<_> = book.spine().iter().map(|e| e.id).collect();
    for chapter_id in chapter_ids {
        let Ok(chapter) = book.load_chapter(chapter_id) else {
            continue;
        };
        for i in 0..chapter.node_count() {
            let id = NodeId(i as u32);
            let Some(node) = chapter.node(id) else {
                continue;
            };
            if node.role != Role::Text {
                continue;
            }
            let len = chapter.text(node.text).chars().count();
            if len == 0 {
                continue;
            }
            match inherited_font_family(&chapter, id) {
                Some(family) => *covered.entry(family.to_string()).or_default() += len,
                // The reader's font — a rival candidate, not a gap.
                None => reader_governed += len,
            }
        }
    }

    // Ties break by name, keeping the choice reproducible across runs.
    let (body, len) = covered
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))?;
    (len > reader_governed).then_some(body)
}

/// Recover the book-level writing mode from the content when the OPF declares
/// no `primary-writing-mode` (see [`book_writing_mode`]), by scanning every
/// chapter's IR style pool. Runs ahead of Pass 2 style ingest.
fn dominant_writing_mode_from_ir(book: &mut Book) -> KfxSymbol {
    use crate::style::WritingMode;
    let mut vrl = 0usize;
    let mut vlr = 0usize;
    let chapter_ids: Vec<_> = book.spine().iter().map(|e| e.id).collect();
    for chapter_id in chapter_ids {
        let Ok(chapter) = book.load_chapter(chapter_id) else {
            continue;
        };
        for (_, style) in chapter.styles.iter() {
            match style.writing_mode {
                WritingMode::VerticalRl => vrl += 1,
                WritingMode::VerticalLr => vlr += 1,
                WritingMode::HorizontalTb => {}
            }
        }
    }
    pick_document_writing_mode(vrl, vlr)
}

/// Choose the book-level writing mode from the count of vertical styles. Any
/// vertical writing mode is decisive — `horizontal_tb` is the CSS initial value
/// and floods the tally — picking the more-cited vertical axis.
fn pick_document_writing_mode(vrl: usize, vlr: usize) -> KfxSymbol {
    if vrl == 0 && vlr == 0 {
        KfxSymbol::HorizontalTb
    } else if vrl >= vlr {
        KfxSymbol::VerticalRl
    } else {
        KfxSymbol::VerticalLr
    }
}

/// Build the book navigation fragment, taking fid:off positions from
/// ctx.position_map. Order matches reference KFX: headings, toc, landmarks.
fn build_book_navigation_fragment_with_positions(book: &Book, ctx: &ExportContext) -> KfxFragment {
    let mut nav_containers = Vec::new();

    // 0. page_list nav container FIRST (Amazon's order): printed page label →
    //    content position, one flat nav_unit per page. Empty where the source
    //    carries no `<nav epub:type="page-list">`.
    let page_list_entries = build_page_list_entries(book.page_list(), ctx);
    if !page_list_entries.is_empty() {
        let page_list_container = IonValue::Struct(vec![
            (
                KfxSymbol::NavType as u64,
                IonValue::Symbol(KfxSymbol::PageList as u64),
            ),
            (
                KfxSymbol::NavContainerName as u64,
                IonValue::Symbol(ctx.nav_container_symbols.page_list),
            ),
            (KfxSymbol::Entries as u64, IonValue::List(page_list_entries)),
        ]);
        let annotated = IonValue::Annotated(
            vec![KfxSymbol::NavContainer as u64],
            Box::new(page_list_container),
        );
        nav_containers.push(annotated);
    }

    // 1. Add headings nav container (first, per reference KFX order)
    let headings_entries = build_headings_entries(ctx);
    let headings_container = IonValue::Struct(vec![
        (
            KfxSymbol::NavType as u64,
            IonValue::Symbol(KfxSymbol::Headings as u64),
        ),
        (
            KfxSymbol::NavContainerName as u64,
            IonValue::Symbol(ctx.nav_container_symbols.headings),
        ),
        (KfxSymbol::Entries as u64, IonValue::List(headings_entries)),
    ]);
    let annotated = IonValue::Annotated(
        vec![KfxSymbol::NavContainer as u64],
        Box::new(headings_container),
    );
    nav_containers.push(annotated);

    // 2. TOC nav container. Amazon lists the cover (表紙) first: a source cover
    //    entry at a content eid is dropped and its children kept, then the
    //    canonical cover → section root is prepended with the landmark's id.
    let src_toc = strip_cover_entries(book.toc(), &cover_section_eids(ctx), cover_label(book), ctx);
    let mut toc_entries = build_toc_entries_with_positions(&src_toc, ctx);
    if let Some(cover_entry) = build_cover_toc_entry(book, ctx) {
        toc_entries.insert(0, cover_entry);
    }
    if !toc_entries.is_empty() {
        let toc_container = IonValue::Struct(vec![
            (
                KfxSymbol::NavType as u64,
                IonValue::Symbol(KfxSymbol::Toc as u64),
            ),
            (
                KfxSymbol::NavContainerName as u64,
                IonValue::Symbol(ctx.nav_container_symbols.toc),
            ),
            (KfxSymbol::Entries as u64, IonValue::List(toc_entries)),
        ]);
        let annotated = IonValue::Annotated(
            vec![KfxSymbol::NavContainer as u64],
            Box::new(toc_container),
        );
        nav_containers.push(annotated);
    }

    // 3. Add landmarks nav container (cover_page and start reading location)
    let landmarks_entries = build_landmarks_entries(book, ctx);
    if !landmarks_entries.is_empty() {
        let landmarks_container = IonValue::Struct(vec![
            (
                KfxSymbol::NavType as u64,
                IonValue::Symbol(KfxSymbol::Landmarks as u64),
            ),
            (
                KfxSymbol::NavContainerName as u64,
                IonValue::Symbol(ctx.nav_container_symbols.landmarks),
            ),
            (KfxSymbol::Entries as u64, IonValue::List(landmarks_entries)),
        ]);
        let annotated = IonValue::Annotated(
            vec![KfxSymbol::NavContainer as u64],
            Box::new(landmarks_container),
        );
        nav_containers.push(annotated);
    }

    // Wrap in reading order structure: [{reading_order_name, nav_containers}]
    let reading_order = IonValue::Struct(vec![
        (
            KfxSymbol::ReadingOrderName as u64,
            IonValue::Symbol(KfxSymbol::Default as u64),
        ),
        (
            KfxSymbol::NavContainers as u64,
            IonValue::List(nav_containers),
        ),
    ]);

    let book_nav = IonValue::List(vec![reading_order]);

    KfxFragment::singleton(KfxSymbol::BookNavigation, book_nav)
}

/// Build headings navigation entries grouped by heading level: one nav_unit per
/// level (h2, h3, …), nesting that level's headings.
fn build_headings_entries(ctx: &ExportContext) -> Vec<IonValue> {
    use std::collections::BTreeMap;

    // Group headings by level
    let mut by_level: BTreeMap<u8, Vec<&crate::formats::kfx::context::HeadingPosition>> =
        BTreeMap::new();
    for heading in &ctx.heading_positions {
        by_level.entry(heading.level).or_default().push(heading);
    }

    // Convert heading level to KFX symbol
    fn level_to_symbol(level: u8) -> Option<KfxSymbol> {
        match level {
            2 => Some(KfxSymbol::H2),
            3 => Some(KfxSymbol::H3),
            4 => Some(KfxSymbol::H4),
            5 => Some(KfxSymbol::H5),
            6 => Some(KfxSymbol::H6),
            _ => None, // h1 not typically used in body
        }
    }

    let mut entries = Vec::new();

    for (level, headings) in by_level {
        let Some(level_symbol) = level_to_symbol(level) else {
            continue;
        };

        if headings.is_empty() {
            continue;
        }

        // Build nested entries for each heading of this level
        let nested_entries: Vec<IonValue> = headings
            .iter()
            .map(|h| {
                IonValue::Annotated(
                    vec![KfxSymbol::NavUnit as u64],
                    Box::new(IonValue::Struct(vec![
                        (
                            KfxSymbol::Representation as u64,
                            IonValue::Struct(vec![(
                                KfxSymbol::Label as u64,
                                IonValue::String("heading-nav-unit".to_string()),
                            )]),
                        ),
                        (
                            KfxSymbol::TargetPosition as u64,
                            IonValue::Struct(vec![
                                (KfxSymbol::Id as u64, IonValue::Int(h.fragment_id as i64)),
                                (KfxSymbol::Offset as u64, IonValue::Int(h.offset as i64)),
                            ]),
                        ),
                    ])),
                )
            })
            .collect();

        // Use first heading's position for the level entry
        let first = headings[0];

        // Build the level entry with nested headings
        let level_entry = IonValue::Annotated(
            vec![KfxSymbol::NavUnit as u64],
            Box::new(IonValue::Struct(vec![
                (
                    KfxSymbol::LandmarkType as u64,
                    IonValue::Symbol(level_symbol as u64),
                ),
                (
                    KfxSymbol::Representation as u64,
                    IonValue::Struct(vec![(
                        KfxSymbol::Label as u64,
                        IonValue::String("heading-nav-unit".to_string()),
                    )]),
                ),
                (
                    KfxSymbol::TargetPosition as u64,
                    IonValue::Struct(vec![
                        (
                            KfxSymbol::Id as u64,
                            IonValue::Int(first.fragment_id as i64),
                        ),
                        (KfxSymbol::Offset as u64, IonValue::Int(first.offset as i64)),
                    ]),
                ),
                (KfxSymbol::Entries as u64, IonValue::List(nested_entries)),
            ])),
        );

        entries.push(level_entry);
    }

    entries
}

/// Build landmark nav entries from `ctx.landmark_fragments`, each converted to a
/// KFX nav_unit through the schema's type mapping.
fn build_landmarks_entries(book: &Book, ctx: &ExportContext) -> Vec<IonValue> {
    use crate::formats::kfx::schema::schema;

    let mut entries = Vec::new();

    // Sort landmarks: Cover, StartReading, then the rest. `landmark_fragments`
    // is a HashMap; the key is TOTAL — reading position then landmark type.
    let mut landmarks: Vec<_> = ctx.landmark_fragments.iter().collect();
    landmarks.sort_by_key(|(lt, target)| {
        let rank = match lt {
            LandmarkType::Cover => 0u8,
            LandmarkType::StartReading => 1,
            _ => 2,
        };
        (rank, target.fragment_id, target.offset, **lt)
    });

    for (landmark_type, target) in landmarks {
        // Convert IR landmark type to KFX symbol via schema
        let Some(kfx_symbol) = schema().landmark_to_kfx(*landmark_type) else {
            continue; // Skip landmarks with no KFX equivalent
        };

        // The cover TOC entry and the `cover_page` landmark share one section
        // id; the device merges them and shows the LANDMARK's label. The
        // localized cover word (表紙) matches the TOC entry.
        let label = if *landmark_type == LandmarkType::Cover {
            cover_label(book).to_string()
        } else {
            target.label.clone()
        };

        let entry = IonValue::Annotated(
            vec![KfxSymbol::NavUnit as u64],
            Box::new(IonValue::Struct(vec![
                (
                    KfxSymbol::LandmarkType as u64,
                    IonValue::Symbol(kfx_symbol as u64),
                ),
                (
                    KfxSymbol::Representation as u64,
                    IonValue::Struct(vec![(KfxSymbol::Label as u64, IonValue::String(label))]),
                ),
                (
                    KfxSymbol::TargetPosition as u64,
                    IonValue::Struct(vec![
                        (
                            KfxSymbol::Id as u64,
                            IonValue::Int(target.fragment_id as i64),
                        ),
                        (
                            KfxSymbol::Offset as u64,
                            IonValue::Int(target.offset as i64),
                        ),
                    ]),
                ),
            ])),
        );
        entries.push(entry);
    }

    entries
}

/// The cover's display word, localized as Amazon does: 表紙 for a Japanese book,
/// else "Cover".
fn cover_label(book: &Book) -> &'static str {
    if book
        .metadata()
        .language
        .to_ascii_lowercase()
        .starts_with("ja")
    {
        "表紙"
    } else {
        "Cover"
    }
}

/// Build the cover's leading TOC nav_unit (表紙 / "Cover Page"), targeting the
/// cover section root — the id the `cover_page` landmark uses, which the device
/// merges with it. `None` without a cover landmark.
fn build_cover_toc_entry(book: &Book, ctx: &ExportContext) -> Option<IonValue> {
    let cover = ctx.landmark_fragments.get(&LandmarkType::Cover)?;
    let target_id = cover.fragment_id;
    let label = cover_label(book);

    let nav_unit = IonValue::Struct(vec![
        (
            KfxSymbol::Representation as u64,
            IonValue::Struct(vec![(
                KfxSymbol::Label as u64,
                IonValue::String(label.to_string()),
            )]),
        ),
        (
            KfxSymbol::TargetPosition as u64,
            IonValue::Struct(vec![
                (KfxSymbol::Id as u64, IonValue::Int(target_id as i64)),
                (KfxSymbol::Offset as u64, IonValue::Int(0)),
            ]),
        ),
    ]);
    Some(IonValue::Annotated(
        vec![KfxSymbol::NavUnit as u64],
        Box::new(nav_unit),
    ))
}

/// EIDs of the cover section (root + content). Strips a cover entry the source
/// TOC carries at a content eid, leaving the canonical one prepended at the
/// section root. Empty where the book has no cover.
fn cover_section_eids(ctx: &ExportContext) -> Vec<u64> {
    let Some(cover) = ctx.landmark_fragments.get(&LandmarkType::Cover) else {
        return Vec::new();
    };
    let mut eids = vec![cover.fragment_id];
    if let Some(cc) = ctx.cover_content_id {
        eids.push(cc);
    }
    if let Some((cid, _)) = ctx
        .chapter_fragments
        .iter()
        .find(|&(_, &fid)| fid == cover.fragment_id)
        && let Some(content) = ctx.content_ids_by_chapter.get(cid)
    {
        eids.extend(content.iter().copied());
    }
    eids
}

/// The source TOC with its own cover entry removed, its children promoted to
/// the level the removed entry held — the rule
/// [`crate::formats::epub::split`]'s carve applies to an out-of-range entry.
fn strip_cover_entries(
    entries: &[crate::model::TocEntry],
    cover_eids: &[u64],
    cover_label: &str,
    ctx: &ExportContext,
) -> Vec<crate::model::TocEntry> {
    let mut out = Vec::new();
    for entry in entries {
        let children = strip_cover_entries(&entry.children, cover_eids, cover_label, ctx);
        if is_cover_entry(entry, cover_eids, cover_label, ctx) {
            out.extend(children);
        } else {
            out.push(crate::model::TocEntry {
                children,
                ..entry.clone()
            });
        }
    }
    out
}

/// Whether a source TOC entry is the book's own cover, by either of the two
/// things that identify one.
fn is_cover_entry(
    entry: &crate::model::TocEntry,
    cover_eids: &[u64],
    cover_label: &str,
    ctx: &ExportContext,
) -> bool {
    // By title: a 表紙 / Cover entry is the source's own cover — catches a
    // round-tripped entry whose href mis-resolves outside the cover section.
    // (Real publisher EPUBs never list the cover in the TOC.)
    let title = entry.title.trim();
    if title == cover_label
        || title.eq_ignore_ascii_case("cover")
        || title.eq_ignore_ascii_case("cover page")
    {
        return true;
    }
    // By position: an entry landing in the cover section is the cover.
    match resolve_toc_target(&entry.target, &entry.href, ctx) {
        Some((fid, _)) => cover_eids.contains(&fid),
        None => false,
    }
}

/// Build TOC entries recursively with anchor entity IDs, pointing at content
/// fragment IDs with offset 0. `resolve_links()` pre-resolves `entry.target`.
/// An entry whose target won't resolve is dropped; its children take its place.
fn build_toc_entries_with_positions(
    entries: &[crate::model::TocEntry],
    ctx: &ExportContext,
) -> Vec<IonValue> {
    let mut out = Vec::new();
    for entry in entries {
        let child_entries = build_toc_entries_with_positions(&entry.children, ctx);

        // Use pre-resolved target to look up position
        let Some((fragment_id, offset)) = resolve_toc_target(&entry.target, &entry.href, ctx)
        else {
            out.extend(child_entries);
            continue;
        };

        let mut fields = Vec::new();

        // Add representation with label
        let representation = IonValue::Struct(vec![(
            KfxSymbol::Label as u64,
            IonValue::String(entry.title.clone()),
        )]);
        fields.push((KfxSymbol::Representation as u64, representation));

        // Target position points directly to content fragment
        let target = IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Int(fragment_id as i64)),
            (KfxSymbol::Offset as u64, IonValue::Int(offset as i64)),
        ]);
        fields.push((KfxSymbol::TargetPosition as u64, target));

        if !child_entries.is_empty() {
            fields.push((KfxSymbol::Entries as u64, IonValue::List(child_entries)));
        }

        let nav_unit = IonValue::Struct(fields);
        // Annotate with nav_unit::
        out.push(IonValue::Annotated(
            vec![KfxSymbol::NavUnit as u64],
            Box::new(nav_unit),
        ));
    }
    out
}

/// Resolve a TOC entry's pre-resolved target to (fragment_id, offset).
///
/// Uses the target from `resolve_links()` to look up the content position.
fn resolve_toc_target(
    target: &Option<AnchorTarget>,
    href: &str,
    ctx: &ExportContext,
) -> Option<(u64, usize)> {
    match target {
        Some(AnchorTarget::Internal(gid)) => {
            // Look up node position - TOC always uses offset 0 (Kindle requirement)
            if let Some((fragment_id, _offset)) = ctx.anchor_registry.get_node_position(*gid) {
                return Some((fragment_id, 0));
            }
            // Body-level ids (promoted to NodeId::ROOT by html::transform) have
            // no element of their own to anchor to. Fall back to chapter start.
            if gid.node == crate::model::NodeId::ROOT
                && let Some(fragment_id) = ctx.anchor_registry.get_chapter_position(gid.chapter)
            {
                return Some((fragment_id, 0));
            }
        }
        Some(AnchorTarget::Chapter(chapter_id)) => {
            // Look up chapter position
            if let Some(fragment_id) = ctx.anchor_registry.get_chapter_position(*chapter_id) {
                return Some((fragment_id, 0));
            }
        }
        Some(AnchorTarget::External(_)) => {
            // External links in TOC - shouldn't happen but handle gracefully
            return None;
        }
        None => {}
    }

    eprintln!("Warning: TOC href not resolved: {}", href);
    None
}

/// Build flat `page_list` nav entries — one `nav_unit` per physical page,
/// Amazon's `nav_unit::{representation:{label}, target_position:{id, offset}}`.
/// An href that doesn't resolve is dropped.
fn build_page_list_entries(
    entries: &[crate::model::TocEntry],
    ctx: &ExportContext,
) -> Vec<IonValue> {
    entries
        .iter()
        .filter_map(|entry| {
            let (fragment_id, offset) = resolve_page_target(&entry.target, &entry.href, ctx)?;

            let representation = IonValue::Struct(vec![(
                KfxSymbol::Label as u64,
                IonValue::String(entry.title.clone()),
            )]);
            let target = IonValue::Struct(vec![
                (KfxSymbol::Id as u64, IonValue::Int(fragment_id as i64)),
                (KfxSymbol::Offset as u64, IonValue::Int(offset as i64)),
            ]);
            let nav_unit = IonValue::Struct(vec![
                (KfxSymbol::Representation as u64, representation),
                (KfxSymbol::TargetPosition as u64, target),
            ]);
            Some(IonValue::Annotated(
                vec![KfxSymbol::NavUnit as u64],
                Box::new(nav_unit),
            ))
        })
        .collect()
}

/// Resolve a page-list entry's target to `(fragment_id, offset)`, preserving the
/// byte offset — the difference from [`resolve_toc_target`], which forces 0. A
/// body-level id or fragment-less href falls back to the chapter start.
fn resolve_page_target(
    target: &Option<AnchorTarget>,
    href: &str,
    ctx: &ExportContext,
) -> Option<(u64, usize)> {
    match target {
        Some(AnchorTarget::Internal(gid)) => {
            if let Some((fragment_id, offset)) = ctx.anchor_registry.get_node_position(*gid) {
                return Some((fragment_id, offset));
            }
            // Body-level ids (promoted to NodeId::ROOT by html::transform) have no
            // element of their own — land on the chapter start.
            if gid.node == crate::model::NodeId::ROOT
                && let Some(fragment_id) = ctx.anchor_registry.get_chapter_position(gid.chapter)
            {
                return Some((fragment_id, 0));
            }
        }
        Some(AnchorTarget::Chapter(chapter_id)) => {
            if let Some(fragment_id) = ctx.anchor_registry.get_chapter_position(*chapter_id) {
                return Some((fragment_id, 0));
            }
        }
        Some(AnchorTarget::External(_)) => return None,
        None => {}
    }

    eprintln!("Warning: page-list href not resolved: {}", href);
    None
}

// Entity Assembler: Packages Schema output into KFX Entity Hierarchy

/// The background half of `<body>`'s style, when it declares a picture. `None`
/// for the ordinary chapter whose body paints nothing.
fn page_background_style(chapter: &Chapter) -> Option<crate::style::ComputedStyle> {
    let root = chapter.styles.get(chapter.node(chapter.root())?.style)?;
    root.background_image.as_ref()?;
    Some(crate::style::ComputedStyle {
        background_image: root.background_image.clone(),
        background_repeat: root.background_repeat,
        background_position_x: root.background_position_x,
        background_position_y: root.background_position_y,
        background_size: root.background_size,
        background_color: root.background_color,
        ..Default::default()
    })
}

/// Build chapter entities separately for grouped emission:
/// (section, storyline, Option<content>).
fn build_chapter_entities_grouped(
    chapter: &Chapter,
    chapter_id: ChapterId,
    section_name: &str,
    ctx: &mut ExportContext,
) -> (KfxFragment, KfxFragment, Option<KfxFragment>) {
    use crate::formats::kfx::storyline::{ir_to_tokens, tokens_to_ion};

    // A cover chapter (image-only) passes three gates: no standalone c0 (a set
    // `cover_fragment_id` owns the cover), one Image node and no text, and no
    // in-spine cover claimed yet.
    let is_cover = ctx.cover_fragment_id.is_none()
        && !ctx.inline_cover_emitted
        && is_image_only_chapter(chapter);

    // A page holding one image and declaring its own pixel box is a full-page
    // illustration or a two-page spread.
    let is_full_page_illustration = !is_cover
        && is_image_only_chapter(chapter)
        && ctx
            .page_viewports
            .get(&chapter_id)
            .is_some_and(|&(w, h)| w > 0 && h > 0);

    // 1. SETUP: Naming for this chapter's entity triad
    let story_name = format!("story_{}", section_name);
    let content_name = format!("content_{}", section_name);

    let section_name_symbol = ctx.symbols.get_or_intern(section_name);
    let story_name_symbol = ctx.symbols.get_or_intern(&story_name);
    let content_name_symbol = ctx.symbols.get_or_intern(&content_name);

    // Tell tokens_to_ion what content name to use for references
    ctx.begin_chapter(&content_name);

    // Get the section fragment ID assigned during Pass 1
    let section_id = ctx
        .get_chapter_fragment(chapter_id)
        .unwrap_or_else(|| ctx.next_fragment_id());

    // For an in-spine cover, the `cover_page` landmark takes this section's
    // page-template id (== section_id), a real Amazon KFX's target. The IR
    // landmark resolver defaults it to the storyline id.
    if is_cover && let Some(target) = ctx.landmark_fragments.get_mut(&LandmarkType::Cover) {
        target.fragment_id = section_id;
    }

    // =========================================================================
    // 2. GENERATE: Schema-driven token generation + text/structure split
    // =========================================================================
    let (storyline_content_list, content_strings) = if is_cover {
        ctx.inline_cover_emitted = true;
        // For cover chapters, generate flat storyline with direct image
        let content_list = build_cover_storyline(chapter, ctx);
        let text = ctx.drain_text();
        (content_list, text)
    } else if is_full_page_illustration {
        let content_list = build_illustration_storyline(chapter, ctx);
        let text = ctx.drain_text();
        (content_list, text)
    } else {
        // Normal chapter: full token-based generation
        let tokens = ir_to_tokens(chapter, ctx);
        let content_list = tokens_to_ion(&tokens, ctx);
        let text = ctx.drain_text();
        (content_list, text)
    };

    // 3. ASSEMBLE: Package into three KFX Entities

    // Entity A: CONTENT ($145) - Holds the raw text strings
    let content_fragment = if !content_strings.is_empty() {
        let content_ion = IonValue::Struct(vec![
            (
                KfxSymbol::Name as u64,
                IonValue::Symbol(content_name_symbol),
            ),
            (
                KfxSymbol::ContentList as u64,
                IonValue::List(content_strings.into_iter().map(IonValue::String).collect()),
            ),
        ]);
        Some(KfxFragment::new(
            KfxSymbol::Content,
            &content_name,
            content_ion,
        ))
    } else {
        None
    };

    // Entity B: STORYLINE ($259) - Holds the structure, references Content by name
    let storyline_ion = IonValue::Struct(vec![
        (
            KfxSymbol::StoryName as u64,
            IonValue::Symbol(story_name_symbol),
        ),
        (KfxSymbol::ContentList as u64, storyline_content_list),
    ]);
    let storyline_fragment = KfxFragment::new(KfxSymbol::Storyline, &story_name, storyline_ion);

    // Entity C: SECTION ($260) - Entry point, references Storyline by story_name
    let page_template = if is_cover {
        // Cover page: container sized to the cover image's pixel dimensions,
        // matching the resource exactly as Amazon's encoder does. Falls back to
        // a generic book-cover aspect on a failed Pass-1 probe.
        let (cw, ch) = ctx.cover_dimensions.unwrap_or((1400, 2100));
        IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Int(section_id as i64)),
            (
                KfxSymbol::StoryName as u64,
                IonValue::Symbol(story_name_symbol),
            ),
            (
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::Container as u64),
            ),
            (KfxSymbol::FixedWidth as u64, IonValue::Int(cw as i64)),
            (KfxSymbol::FixedHeight as u64, IonValue::Int(ch as i64)),
            (
                KfxSymbol::Layout as u64,
                IonValue::Symbol(KfxSymbol::ScaleFit as u64),
            ),
            (
                KfxSymbol::Float as u64,
                IonValue::Symbol(KfxSymbol::Center as u64),
            ),
        ])
    } else {
        // Normal text page
        let mut fields = vec![
            (KfxSymbol::Id as u64, IonValue::Int(section_id as i64)),
            (
                KfxSymbol::StoryName as u64,
                IonValue::Symbol(story_name_symbol),
            ),
            (
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::Text as u64),
            ),
        ];
        // A picture `<body>` paints belongs to the page: the storyline walk
        // emits the root's children and never the root. Only a background
        // reaches the page template; body's margins and padding stay behind.
        if let Some(style) = page_background_style(chapter) {
            let symbol = ctx.register_ir_style_with_hint(&style, Some("page"));
            fields.push((KfxSymbol::Style as u64, IonValue::Symbol(symbol)));
        }
        IonValue::Struct(fields)
    };

    let section_ion = IonValue::Struct(vec![
        (
            KfxSymbol::SectionName as u64,
            IonValue::Symbol(section_name_symbol),
        ),
        (
            KfxSymbol::PageTemplates as u64,
            IonValue::List(vec![page_template]),
        ),
    ]);
    let section_fragment =
        KfxFragment::new_with_id(KfxSymbol::Section, section_id, section_name, section_ion);

    (section_fragment, storyline_fragment, content_fragment)
}

/// Storyline for a full-page illustration: a container filling the page holding
/// one centred full-width image. Mirrors the shape Amazon gives an illustration
/// page in a reflowing book.
fn build_illustration_storyline(chapter: &Chapter, ctx: &mut ExportContext) -> IonValue {
    use crate::model::Role;
    use crate::style::{ComputedStyle, Length};

    // The source's own axis, keeping these two styles out of the per-style
    // `writing_mode` overrides a vertical book must not carry.
    let writing_mode = ctx.ir_style_baseline_writing_mode();
    let wrapper_style = ComputedStyle {
        width: Length::Percent(100.0),
        height: Length::Percent(100.0),
        writing_mode,
        ..Default::default()
    };
    // `margin: auto` on both sides resolves to `box_align: center` in the style
    // schema, which needs a definite width beside it.
    let image_style = ComputedStyle {
        width: Length::Percent(100.0),
        margin_left: Length::Auto,
        margin_right: Length::Auto,
        writing_mode,
        ..Default::default()
    };

    for node_id in chapter.iter_dfs() {
        let Some(node) = chapter.node(node_id) else {
            continue;
        };
        if node.role != Role::Image {
            continue;
        }
        let Some(src) = chapter.semantics.src(node_id) else {
            continue;
        };
        let resource_name = ctx.resource_registry.get_or_create_name(src);
        let resource_symbol = ctx.symbols.get_or_intern(&resource_name);
        let wrapper_symbol = ctx.register_ir_style(&wrapper_style);
        let image_symbol = ctx.register_ir_style(&image_style);

        let wrapper_id = ctx.fragment_ids.next_id();
        let image_id = ctx.fragment_ids.next_id();
        ctx.record_content_id(image_id);
        ctx.record_content_length(image_id, 1);
        ctx.resolve_pending_chapter_anchor(wrapper_id);

        let image = IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Int(image_id as i64)),
            (KfxSymbol::Style as u64, IonValue::Symbol(image_symbol)),
            (
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::Image as u64),
            ),
            (
                KfxSymbol::ResourceName as u64,
                IonValue::Symbol(resource_symbol),
            ),
        ]);
        return IonValue::List(vec![IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Int(wrapper_id as i64)),
            (
                KfxSymbol::Layout as u64,
                IonValue::Symbol(KfxSymbol::Vertical as u64),
            ),
            (KfxSymbol::Style as u64, IonValue::Symbol(wrapper_symbol)),
            (
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::Container as u64),
            ),
            (KfxSymbol::ContentList as u64, IonValue::List(vec![image])),
        ])]);
    }
    IonValue::List(Vec::new())
}

/// Build a simplified storyline for cover chapters: the image directly in
/// content_list — `[{ type: image, resource_name, style }]`.
fn build_cover_storyline(chapter: &Chapter, ctx: &mut ExportContext) -> IonValue {
    use crate::model::Role;

    // Find the image node
    for node_id in chapter.iter_dfs() {
        let node = match chapter.node(node_id) {
            Some(n) => n,
            None => continue,
        };

        if node.role == Role::Image {
            // Get the image source
            if let Some(src) = chapter.semantics.src(node_id) {
                // Look up the resource name (e.g., "e0")
                let resource_name = ctx.resource_registry.get_or_create_name(src);
                let resource_name_symbol = ctx.symbols.get_or_intern(&resource_name);

                // Register style and get symbol. Cover image often has a
                // distinctive source class like `p-cover` — passing it as a
                // hint keeps that name in the KFX style symbol table.
                let style_symbol = ctx.register_style_id_with_hint(
                    node.style,
                    &chapter.styles,
                    chapter.semantics.class(node_id),
                );

                // Generate unique container ID
                let container_id = ctx.fragment_ids.next_id();

                // Record content ID for position_map and location_map
                ctx.record_content_id(container_id);
                // Record length of 1 for image (per kfx_output algorithm)
                ctx.record_content_length(container_id, 1);

                // The pending chapter-start anchor, skipped by the cover path
                ctx.resolve_pending_chapter_anchor(container_id);

                // Build the image struct directly (no container wrapper)
                let image_struct = IonValue::Struct(vec![
                    (KfxSymbol::Id as u64, IonValue::Int(container_id as i64)),
                    (KfxSymbol::Style as u64, IonValue::Symbol(style_symbol)),
                    (
                        KfxSymbol::Type as u64,
                        IonValue::Symbol(KfxSymbol::Image as u64),
                    ),
                    (
                        KfxSymbol::ResourceName as u64,
                        IonValue::Symbol(resource_name_symbol),
                    ),
                ]);

                return IonValue::List(vec![image_struct]);
            }
        }
    }

    // Fallback: empty list if no image found
    IonValue::List(vec![])
}

/// Build the three KFX entities for a chapter: Content, Storyline, Section.
/// `ir_to_tokens` and `tokens_to_ion` carry the element semantics; this knows
/// the entity topology only.
#[allow(dead_code)]
fn build_chapter_entities(
    chapter: &Chapter,
    chapter_id: ChapterId,
    section_name: &str,
    ctx: &mut ExportContext,
) -> Vec<KfxFragment> {
    use crate::formats::kfx::storyline::{ir_to_tokens, tokens_to_ion};

    let mut fragments = Vec::new();

    // 1. SETUP: Naming for this chapter's entity triad
    let story_name = format!("story_{}", section_name);
    let content_name = format!("content_{}", section_name);

    let section_name_symbol = ctx.symbols.get_or_intern(section_name);
    let story_name_symbol = ctx.symbols.get_or_intern(&story_name);
    let content_name_symbol = ctx.symbols.get_or_intern(&content_name);

    // Tell tokens_to_ion what content name to use for references
    ctx.begin_chapter(&content_name);

    // Get the section fragment ID assigned during Pass 1
    let section_id = ctx
        .get_chapter_fragment(chapter_id)
        .unwrap_or_else(|| ctx.next_fragment_id());

    // 2. GENERATE: `ir_to_tokens` builds Tokens from the Schema;
    // `tokens_to_ion` splits structure into Ion and text into ctx.text_accumulator
    let tokens = ir_to_tokens(chapter, ctx);
    let storyline_content_list = tokens_to_ion(&tokens, ctx);

    // Drain the accumulated text strings (captured during tokens_to_ion)
    let content_strings = ctx.drain_text();

    // 3. ASSEMBLE: Package into three KFX Entities

    // Entity A: CONTENT ($145) - Holds the raw text strings
    if !content_strings.is_empty() {
        let content_ion = IonValue::Struct(vec![
            (
                KfxSymbol::Name as u64,
                IonValue::Symbol(content_name_symbol),
            ),
            (
                KfxSymbol::ContentList as u64,
                IonValue::List(content_strings.into_iter().map(IonValue::String).collect()),
            ),
        ]);
        fragments.push(KfxFragment::new(
            KfxSymbol::Content,
            &content_name,
            content_ion,
        ));
    }

    // Entity B: STORYLINE ($259) - Holds the structure, references Content by name
    let storyline_ion = IonValue::Struct(vec![
        (
            KfxSymbol::StoryName as u64,
            IonValue::Symbol(story_name_symbol),
        ),
        (KfxSymbol::ContentList as u64, storyline_content_list),
    ]);
    fragments.push(KfxFragment::new(
        KfxSymbol::Storyline,
        &story_name,
        storyline_ion,
    ));

    // Entity C: SECTION ($260) - Entry point, references Storyline by story_name
    let page_template = IonValue::Struct(vec![
        (KfxSymbol::Id as u64, IonValue::Int(section_id as i64)),
        (
            KfxSymbol::StoryName as u64,
            IonValue::Symbol(story_name_symbol),
        ),
        (
            KfxSymbol::Type as u64,
            IonValue::Symbol(KfxSymbol::Text as u64),
        ),
    ]);

    let section_ion = IonValue::Struct(vec![
        (
            KfxSymbol::SectionName as u64,
            IonValue::Symbol(section_name_symbol),
        ),
        (
            KfxSymbol::PageTemplates as u64,
            IonValue::List(vec![page_template]),
        ),
    ]);
    fragments.push(KfxFragment::new_with_id(
        KfxSymbol::Section,
        section_id,
        section_name,
        section_ion,
    ));

    fragments
}

/// Build the document symbols section — the local symbol table as
/// `$ion_symbol_table::{imports: [{name, version, max_id}], symbols: [...]}`.
/// Symbol order IS identity: symbol ID = KFX_SYMBOL_TABLE_SIZE + index.
fn build_symbol_table_ion(local_symbols: &[String]) -> Vec<u8> {
    use crate::formats::kfx::ion::IonWriter;
    use crate::formats::kfx::symbols::KFX_MAX_SYMBOL_ID;

    let mut writer = IonWriter::new();
    writer.write_bvm();

    // Build the import entry for YJ_symbols (Amazon's KFX symbol table)
    // { name: "YJ_symbols", version: 10, max_id: 851 }
    let import_entry = IonValue::Struct(vec![
        (4, IonValue::String("YJ_symbols".to_string())), // $4 = name
        (5, IonValue::Int(10)),                          // $5 = version
        (8, IonValue::Int(KFX_MAX_SYMBOL_ID as i64)),    // $8 = max_id
    ]);

    // Build the symbols list with local symbols
    let symbols_list: Vec<IonValue> = local_symbols
        .iter()
        .map(|s| IonValue::String(s.clone()))
        .collect();

    // Build the $ion_symbol_table struct
    // { imports: [...], symbols: [...] }
    let symbol_table = IonValue::Struct(vec![
        (6, IonValue::List(vec![import_entry])), // $6 = imports
        (7, IonValue::List(symbols_list)),       // $7 = symbols
    ]);

    // Write with $ion_symbol_table annotation ($3)
    writer.write_annotated(&[3], &symbol_table);

    writer.into_bytes()
}

/// Build format capabilities ION.
fn build_format_capabilities_ion() -> Vec<u8> {
    let caps = IonValue::Struct(vec![
        (
            KfxSymbol::Namespace as u64,
            IonValue::String("yj".to_string()),
        ),
        (KfxSymbol::MajorVersion as u64, IonValue::Int(1)),
        (KfxSymbol::MinorVersion as u64, IonValue::Int(0)),
        (KfxSymbol::Features as u64, IonValue::List(vec![])),
    ]);

    // Annotate with $593 (format_capabilities)
    serialize_annotated_ion(KfxSymbol::FormatCapabilities as u64, &caps)
}

/// Build an external_resource fragment ($164) - metadata about a resource.
/// Default per-band quantizer for grayscale-JXR plates: ~Amazon's per-image
/// size on LN content at high fidelity (the `8/16/32` point of a QP sweep).
const JXR_DEFAULT_QP: jxr::QpSet = jxr::QpSet {
    dc: 8,
    lp: 16,
    hp: 32,
};

/// Prepare the cover image's bytes for KFX bundling as JPEG. An SVG cover is
/// rasterized first — KFX has no vector resource format — then takes the same
/// JFIF path as any other. `None` means the bytes stand as they are.
fn cover_jpeg_for_kfx(data: &[u8]) -> Option<Vec<u8>> {
    #[cfg(feature = "svg")]
    if let Some(img) = crate::image::svg::rasterize(data) {
        return crate::image::jpeg::encode_as_jpeg(&img);
    }
    crate::image::jpeg::sanitize_for_kfx(data)
}

/// Reject vector art a build without the `svg` feature cannot rasterize. KFX
/// carries no vector resource format: the verbatim bytes reach the device as a
/// resource that draws nothing.
#[cfg(not(feature = "svg"))]
fn reject_unrasterizable_svg(href: &str, data: &[u8]) -> io::Result<()> {
    if crate::image::svg::looks_like_svg(data) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{href}: SVG art needs the `svg` feature — KFX has no vector resource format"),
        ));
    }
    Ok(())
}

#[cfg(feature = "svg")]
fn reject_unrasterizable_svg(_href: &str, _data: &[u8]) -> io::Result<()> {
    Ok(())
}

/// Prepare a media asset's bytes for KFX bundling: a raster image is re-encoded
/// as grayscale JPEG-XR, Amazon's own image codec, and an SVG is rasterized onto
/// the same path. A font or an encode failure takes the JPEG sanitize path.
fn encode_asset_for_kfx(data: &[u8], mode: jxr::ColorMode) -> Vec<u8> {
    if let Some(jxr) = encode_jxr_asset(data, mode) {
        return jxr;
    }
    #[cfg(feature = "svg")]
    if let Some(img) = crate::image::svg::rasterize(data)
        && let Some(jxr) = encode_dynimg_jxr(&img, mode)
    {
        return jxr;
    }
    crate::image::jpeg::sanitize_for_kfx(data).unwrap_or_else(|| data.to_vec())
}

/// Decode a raster image and re-encode it as JPEG-XR in the requested
/// [`ColorMode`][jxr::ColorMode]: `8bppGray` or `24bppRGB`. `None` for bytes that
/// aren't a decodable raster or exceed the encoder's range.
fn encode_jxr_asset(data: &[u8], mode: jxr::ColorMode) -> Option<Vec<u8>> {
    let img = ::image::load_from_memory(data).ok()?;
    encode_dynimg_jxr(&img, mode)
}

/// Encode a decoded raster as JPEG-XR in the requested
/// [`ColorMode`][jxr::ColorMode]. Shared by [`encode_jxr_asset`] and the
/// fixed-layout manga thumbnailer.
fn encode_dynimg_jxr(img: &::image::DynamicImage, mode: jxr::ColorMode) -> Option<Vec<u8>> {
    use jxr::{ColorMode, ImageInput, encode};
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 || w > (1 << 16) || h > (1 << 16) {
        return None;
    }
    // Flatten alpha over white first, matching the JPEG path's
    // `flatten_to_rgb`: the plate formats here (8bppGray, 24bppRGB) carry none,
    // and `to_luma8`/`to_rgb8` drop the channel without compositing.
    let flattened;
    let img = if img.color().has_alpha() {
        flattened = crate::image::jpeg::flatten_alpha_over_white(img);
        &flattened
    } else {
        img
    };
    let planes: Vec<Vec<u8>> = match mode {
        ColorMode::Grayscale => vec![img.to_luma8().into_raw()],
        ColorMode::Color => {
            // De-interleave RGB8 into three planar channels (`ImageInput` layout).
            let raw = img.to_rgb8().into_raw();
            let n = (w * h) as usize;
            let (mut r, mut g, mut b) = (vec![0u8; n], vec![0u8; n], vec![0u8; n]);
            for i in 0..n {
                r[i] = raw[i * 3];
                g[i] = raw[i * 3 + 1];
                b[i] = raw[i * 3 + 2];
            }
            vec![r, g, b]
        }
        // The gray/RGB raster modes are the ones wired here. A new ColorMode
        // reaching this site trips the debug_assert.
        _ => {
            debug_assert!(false, "encode_jxr_asset: unhandled ColorMode {mode:?}");
            return None;
        }
    };
    let input = ImageInput {
        width: w,
        height: h,
        planes: &planes,
        premultiplied_alpha: false,
    };
    encode(&input, mode, JXR_DEFAULT_QP).ok()
}

fn build_external_resource_fragment(
    href: &str,
    data: &[u8],
    ctx: &mut ExportContext,
) -> KfxFragment {
    // Generate a short resource name (e.g., "e0", "e1", etc.)
    let resource_name = generate_resource_name(href, ctx);
    let resource_name_symbol = ctx.symbols.get_or_intern(&resource_name);

    let mut fields = Vec::new();

    // resource_name - the symbolic name for this resource
    fields.push((
        KfxSymbol::ResourceName as u64,
        IonValue::Symbol(resource_name_symbol),
    ));

    // location - path to the bcRawMedia entity
    let location = format!("resource/{}", resource_name);
    fields.push((KfxSymbol::Location as u64, IonValue::String(location)));

    // format - file type symbol
    let format_symbol = detect_format_symbol(href, data);
    fields.push((KfxSymbol::Format as u64, IonValue::Symbol(format_symbol)));

    // For images, try to extract dimensions
    if let Some((width, height)) = crate::util::extract_image_dimensions(data) {
        fields.push((KfxSymbol::ResourceWidth as u64, IonValue::Int(width as i64)));
        fields.push((
            KfxSymbol::ResourceHeight as u64,
            IonValue::Int(height as i64),
        ));
    }

    // mime type for images
    if let Some(mime) = crate::util::detect_mime_type(href, data) {
        fields.push((KfxSymbol::Mime as u64, IonValue::String(mime.to_string())));
    }

    let ion = IonValue::Struct(fields);
    KfxFragment::new(KfxSymbol::ExternalResource, &resource_name, ion)
}

/// Build a resource fragment (bcRawMedia $417) - the actual bytes.
fn build_resource_fragment(href: &str, data: &[u8], ctx: &mut ExportContext) -> KfxFragment {
    // Use resource/ prefix to distinguish from external_resource fragment
    // This ensures bcRawMedia gets a different entity ID
    let resource_name = generate_resource_name(href, ctx);
    let raw_name = format!("resource/{}", resource_name);

    // Register the prefixed name as a symbol
    ctx.symbols.get_or_intern(&raw_name);

    // Create raw fragment for binary resources
    KfxFragment::raw(KfxSymbol::Bcrawmedia as u64, &raw_name, data.to_vec())
}

/// Build a font resource fragment (bcRawFont $418) — a typeface's raw bytes, with
/// no `external_resource` beside it: that fragment describes a picture and KFX's
/// format enum has no font member. Amazon writes bcRawFont plus a `font` ($262).
fn build_font_resource_fragment(href: &str, data: &[u8], ctx: &mut ExportContext) -> KfxFragment {
    let resource_name = generate_resource_name(href, ctx);
    let raw_name = format!("resource/{}", resource_name);
    ctx.symbols.get_or_intern(&raw_name);
    KfxFragment::raw(KfxSymbol::Bcrawfont as u64, &raw_name, data.to_vec())
}

/// Build font entity fragments ($262) from @font-face rules, linking
/// font_family names to resource locations.
fn build_font_fragments(book: &mut Book, ctx: &mut ExportContext) -> Vec<KfxFragment> {
    use crate::style::{FontStyle, FontWeight};

    let mut fragments = Vec::new();
    let font_faces = book.font_faces();

    for font_face in font_faces {
        // Check if the font file exists as a resource
        let resource_name = match ctx.resource_registry.get_name(&font_face.src) {
            Some(name) => name.to_string(),
            None => {
                // Try without leading path components
                let filename = std::path::Path::new(&font_face.src)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&font_face.src);

                // The matching resource, lexicographically smallest of the
                // matches — a deterministic pick, not HashMap order.
                let found = ctx
                    .resource_registry
                    .iter()
                    .map(|(href, _)| href)
                    .filter(|href| href.ends_with(filename))
                    .min()
                    .and_then(|href| ctx.resource_registry.get_name(href))
                    .map(|s| s.to_string());
                match found {
                    Some(name) => name,
                    None => continue, // Skip if font file not found
                }
            }
        };

        // Build location path
        let location = format!("resource/{}", resource_name);

        // The original font family name, as styles reference it
        let font_family = font_face.font_family.clone();

        // Convert font_weight to KFX symbol
        let weight_symbol = match font_face.font_weight {
            FontWeight(w) if w >= 700 => KfxSymbol::Bold,
            _ => KfxSymbol::Normal,
        };

        // Convert font_style to KFX symbol
        let style_symbol = match font_face.font_style {
            FontStyle::Italic | FontStyle::Oblique => KfxSymbol::Italic,
            FontStyle::Normal => KfxSymbol::Normal,
        };

        // Build font entity ION structure
        let ion = IonValue::Struct(vec![
            (
                KfxSymbol::FontFamily as u64,
                IonValue::String(font_family.clone()),
            ),
            (
                KfxSymbol::FontStyle as u64,
                IonValue::Symbol(style_symbol as u64),
            ),
            (KfxSymbol::Location as u64, IonValue::String(location)),
            (
                KfxSymbol::FontWeight as u64,
                IonValue::Symbol(weight_symbol as u64),
            ),
            (
                KfxSymbol::FontStretch as u64,
                IonValue::Symbol(KfxSymbol::Normal as u64),
            ),
        ]);

        // Generate unique fragment name for this font face
        let frag_name = format!(
            "font-{}-{}-{}",
            font_face.font_family,
            if font_face.font_weight.0 >= 700 {
                "bold"
            } else {
                "normal"
            },
            match font_face.font_style {
                FontStyle::Italic | FontStyle::Oblique => "italic",
                FontStyle::Normal => "normal",
            }
        );

        fragments.push(KfxFragment::new(KfxSymbol::Font, &frag_name, ion));
    }

    fragments
}

/// Build anchor fragments ($266) for every recorded anchor. Returns
/// (fragments, anchor_ids_by_fragment), the latter fragment_id → anchor symbol
/// IDs for position_map.
fn build_anchor_fragments(ctx: &mut ExportContext) -> (Vec<KfxFragment>, HashMap<u64, Vec<u64>>) {
    let mut fragments = Vec::new();
    let mut anchor_ids_by_fragment: HashMap<u64, Vec<u64>> = HashMap::new();

    // Get resolved internal anchors from the AnchorRegistry
    let resolved_anchors = ctx.anchor_registry.drain_anchors();

    for anchor in resolved_anchors {
        // Intern the anchor symbol to get its ID
        let anchor_symbol_id = ctx.symbols.get_or_intern(&anchor.symbol);

        // Track which anchors belong to which SECTION for position_map
        // Key by section_id (page_template ID), not fragment_id (content ID)
        anchor_ids_by_fragment
            .entry(anchor.section_id)
            .or_default()
            .push(anchor_symbol_id);

        // Build position struct - uses content fragment_id for navigation target
        let mut pos_fields = Vec::new();
        pos_fields.push((
            KfxSymbol::Id as u64,
            IonValue::Int(anchor.fragment_id as i64),
        ));
        // Only include offset when non-zero - reference KFX omits offset for fragment-only positions
        if anchor.offset > 0 {
            pos_fields.push((
                KfxSymbol::Offset as u64,
                IonValue::Int(anchor.offset as i64),
            ));
        }

        let ion = IonValue::Struct(vec![
            (
                KfxSymbol::AnchorName as u64,
                IonValue::Symbol(anchor_symbol_id),
            ),
            (KfxSymbol::Position as u64, IonValue::Struct(pos_fields)),
        ]);

        fragments.push(KfxFragment::new(KfxSymbol::Anchor, &anchor.symbol, ion));
    }

    // Get external anchors (http/https links) from the AnchorRegistry
    let external_anchors = ctx.anchor_registry.drain_external_anchors();

    for anchor in external_anchors {
        // Intern the anchor symbol to get its ID
        let anchor_symbol_id = ctx.symbols.get_or_intern(&anchor.symbol);

        // An external anchor carries uri, not position
        let ion = IonValue::Struct(vec![
            (KfxSymbol::Uri as u64, IonValue::String(anchor.uri.clone())),
            (
                KfxSymbol::AnchorName as u64,
                IonValue::Symbol(anchor_symbol_id),
            ),
        ]);

        fragments.push(KfxFragment::new(KfxSymbol::Anchor, &anchor.symbol, ion));
    }

    (fragments, anchor_ids_by_fragment)
}

/// Generate a short resource name for a given href.
fn generate_resource_name(href: &str, ctx: &mut ExportContext) -> String {
    ctx.resource_registry.get_or_create_name(href)
}

// Navigation Maps ($264, $265, $550)

/// Build position_map fragment ($264): section → the EIDs it contains, which the
/// Kindle reader walks to place a position.
fn build_position_map_fragment(
    ctx: &ExportContext,
    _anchor_ids_by_fragment: &HashMap<u64, Vec<u64>>,
) -> KfxFragment {
    let mut entries = Vec::new();

    // Handle standalone cover section (c0) if present
    // Cover contains both the page_template ID and the storyline content ID
    let section_offset = if let Some(cover_fid) = ctx.cover_fragment_id {
        // Build contains list: [section_id, content_id]
        let mut contains_list = vec![IonValue::Int(cover_fid as i64)];
        if let Some(content_id) = ctx.cover_content_id {
            contains_list.push(IonValue::Int(content_id as i64));
        }
        let entry = IonValue::Struct(vec![
            (KfxSymbol::Contains as u64, IonValue::List(contains_list)),
            (
                KfxSymbol::SectionName as u64,
                IonValue::Symbol(ctx.section_ids[0]),
            ),
        ]);
        entries.push(entry);
        1 // Skip c0 when processing spine chapters
    } else {
        0
    };

    // Build entries for spine chapters (skip cover section if present)
    // Sort chapters by fragment ID to maintain consistent ordering
    let mut chapter_entries: Vec<_> = ctx.chapter_fragments.iter().collect();
    chapter_entries.sort_by_key(|(_, fid)| **fid);

    for (idx, &section_sym) in ctx.section_ids.iter().skip(section_offset).enumerate() {
        if let Some(&(chapter_id, fragment_id)) = chapter_entries.get(idx) {
            let mut eid_list = Vec::new();

            // Include page_template ID first (required for section start images)
            eid_list.push(IonValue::Int(*fragment_id as i64));

            // Add all content fragment IDs for this chapter
            if let Some(content_ids) = ctx.content_ids_by_chapter.get(chapter_id) {
                for &content_id in content_ids {
                    eid_list.push(IonValue::Int(content_id as i64));
                }
            }

            let entry = IonValue::Struct(vec![
                (KfxSymbol::Contains as u64, IonValue::List(eid_list)),
                (KfxSymbol::SectionName as u64, IonValue::Symbol(section_sym)),
            ]);
            entries.push(entry);
        }
    }

    let ion = IonValue::List(entries);
    KfxFragment::singleton(KfxSymbol::PositionMap, ion)
}

/// One section's reading-position layout: name (keys `section_position_id_map`),
/// symbol (read inside `position_id_map`), and its EIDs in reading order with
/// each span. The first EID is the section root, span 1.
struct SectionPos {
    name: String,
    sym: u64,
    /// `(eid, span)` in reading order, section root first.
    eids: Vec<(u64, i64)>,
}

/// Assemble the per-section position layout — the source of truth for
/// `position_id_map`, `section_position_id_map` and the navigable section roots.
/// `section_names` keys those entities, Amazon's per-section-name shape.
fn section_positions(ctx: &ExportContext, section_names: &[String]) -> Vec<SectionPos> {
    let span = |eid: u64| -> i64 {
        // Section roots / images aren't in `content_id_lengths` ⇒ span 1; text
        // content advances by its UTF-16 length.
        ctx.content_id_lengths
            .get(&eid)
            .copied()
            .unwrap_or(1)
            .max(1) as i64
    };

    let mut out: Vec<SectionPos> = Vec::new();

    // Standalone cover section (c0) first, if present.
    let section_offset = if let Some(root) = ctx.cover_fragment_id {
        let mut eids = vec![(root, span(root))];
        if let Some(cc) = ctx.cover_content_id {
            eids.push((cc, span(cc)));
        }
        out.push(SectionPos {
            name: section_names[0].clone(),
            sym: ctx.section_ids[0],
            eids,
        });
        1
    } else {
        0
    };

    // Spine chapters in fragment-id order, a first-spine-doc cover included
    let mut chapters: Vec<_> = ctx.chapter_fragments.iter().collect();
    chapters.sort_by_key(|(_, fid)| **fid);
    for (idx, &sym) in ctx.section_ids.iter().skip(section_offset).enumerate() {
        let Some(&(chapter_id, &root)) = chapters.get(idx) else {
            continue;
        };
        let mut eids = vec![(root, span(root))];
        if let Some(content) = ctx.content_ids_by_chapter.get(chapter_id) {
            eids.extend(content.iter().map(|&c| (c, span(c))));
        }
        out.push(SectionPos {
            name: section_names[section_offset + idx].clone(),
            sym,
            eids,
        });
    }

    out
}

/// position_id_map ($265), Amazon's section-keyed reflowable shape:
/// `{contains: [{section_name, pid, length}, …]}`, where `length` agrees with the
/// paired `section_position_id_map` terminator. Section roots stay navigable.
fn build_position_id_map_fragment(secs: &[SectionPos]) -> KfxFragment {
    let mut entries = Vec::with_capacity(secs.len());
    let mut pid = 0i64;
    for s in secs {
        let length: i64 = s.eids.iter().map(|&(_, span)| span).sum();
        entries.push(IonValue::Struct(vec![
            (KfxSymbol::SectionName as u64, IonValue::Symbol(s.sym)),
            (KfxSymbol::Pid as u64, IonValue::Int(pid)),
            (KfxSymbol::Length as u64, IonValue::Int(length)),
        ]));
        pid += length;
    }
    let ion = IonValue::Struct(vec![(KfxSymbol::Contains as u64, IonValue::List(entries))]);
    KfxFragment::singleton(KfxSymbol::PositionIdMap, ion)
}

/// section_position_id_map ($609): one entity per section, the compact
/// position→EID walk (a bare int for previous + 1, else `[advance, eid]`, with an
/// `[advance, 0]` terminator at the section length). Keyed by section-name symbol.
fn build_section_position_id_map_fragments(secs: &[SectionPos]) -> Vec<KfxFragment> {
    secs.iter()
        .map(|s| {
            let mut contains: Vec<IonValue> = Vec::with_capacity(s.eids.len() + 1);
            let mut prev: Option<(u64, i64)> = None;
            for &(eid, span) in &s.eids {
                let advance = prev.map_or(0, |(_, sp)| sp);
                let consecutive = prev.is_some_and(|(p, _)| eid == p + 1);
                if consecutive {
                    contains.push(IonValue::Int(advance));
                } else {
                    contains.push(IonValue::List(vec![
                        IonValue::Int(advance),
                        IonValue::Int(eid as i64),
                    ]));
                }
                prev = Some((eid, span));
            }
            let last_span = prev.map_or(0, |(_, sp)| sp);
            contains.push(IonValue::List(vec![
                IonValue::Int(last_span),
                IonValue::Int(0),
            ]));

            let ion = IonValue::Struct(vec![
                (KfxSymbol::SectionName as u64, IonValue::Symbol(s.sym)),
                (KfxSymbol::Contains as u64, IonValue::List(contains)),
            ]);
            KfxFragment::new(KfxSymbol::SectionPositionIdMap, s.name.clone(), ion)
        })
        .collect()
}

/// Build location_map fragment ($550): location number → position, one entry per
/// content block at offset 0, Amazon's format for this entity.
fn build_location_map_fragment(ctx: &ExportContext) -> KfxFragment {
    let mut location_entries = Vec::new();

    // Helper closure to process a single content ID - always offset 0
    let mut process_content_id = |content_id: u64| {
        let entry = IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Int(content_id as i64)),
            (KfxSymbol::Offset as u64, IonValue::Int(0)),
        ]);
        location_entries.push(entry);
    };

    // Process cover content ID first if present
    if let Some(cover_id) = ctx.cover_content_id {
        process_content_id(cover_id);
    }

    // Process chapter content in order (sorted by fragment ID)
    let mut chapter_entries: Vec<_> = ctx.chapter_fragments.iter().collect();
    chapter_entries.sort_by_key(|(_, fid)| **fid);

    for (chapter_id, _) in &chapter_entries {
        if let Some(content_ids) = ctx.content_ids_by_chapter.get(chapter_id) {
            for &content_id in content_ids {
                process_content_id(content_id);
            }
        }
    }

    // Wrap in locations list structure
    let ion = IonValue::List(vec![IonValue::Struct(vec![(
        KfxSymbol::Locations as u64,
        IonValue::List(location_entries),
    )])]);

    KfxFragment::singleton(KfxSymbol::LocationMap, ion)
}

/// Build resource_path fragment ($395), listing additional resource paths. A
/// simple conversion leaves the entries array empty.
fn build_resource_path_fragment() -> KfxFragment {
    let ion = IonValue::Struct(vec![(KfxSymbol::Entries as u64, IonValue::List(vec![]))]);
    KfxFragment::singleton(KfxSymbol::ResourcePath, ion)
}

/// Build container_entity_map fragment ($419): the container's entities plus an
/// `entity_dependencies` graph — section → external_resource → bcRawMedia
/// location.
fn build_container_entity_map_fragment(
    container_id: &str,
    fragments: &[KfxFragment],
    ctx: &ExportContext,
) -> KfxFragment {
    // Every non-singleton entity name symbol, bcRawMedia locations included
    let mut entity_names: Vec<IonValue> = Vec::new();

    for frag in fragments {
        if frag.fid.starts_with('$') {
            continue;
        }
        if let Some(symbol_id) = ctx.symbols.get(&frag.fid) {
            entity_names.push(IonValue::Symbol(symbol_id));
        }
    }

    let container_entry = IonValue::Struct(vec![
        (
            KfxSymbol::Id as u64,
            IonValue::String(container_id.to_string()),
        ),
        (KfxSymbol::Contains as u64, IonValue::List(entity_names)),
    ]);

    // entity_dependencies: section → resource names, external_resource → location
    let mut dependencies: Vec<IonValue> = Vec::new();

    for (section_name, short_names) in &ctx.section_resource_deps {
        if short_names.is_empty() {
            continue;
        }
        let Some(section_sym) = ctx.symbols.get(section_name) else {
            continue;
        };
        let deps: Vec<IonValue> = short_names
            .iter()
            .filter_map(|n| ctx.symbols.get(n).map(IonValue::Symbol))
            .collect();
        if deps.is_empty() {
            continue;
        }
        dependencies.push(IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Symbol(section_sym)),
            (
                KfxSymbol::MandatoryDependencies as u64,
                IonValue::List(deps),
            ),
        ]));
    }

    // Collect every distinct resource short name actually used and emit its
    // bcRawMedia location as a dependency.
    let mut all_short_names: BTreeSet<&String> = BTreeSet::new();
    for short_names in ctx.section_resource_deps.values() {
        for n in short_names {
            all_short_names.insert(n);
        }
    }
    for short_name in all_short_names {
        let Some(resource_sym) = ctx.symbols.get(short_name) else {
            continue;
        };
        let raw_name = format!("resource/{short_name}");
        let Some(raw_sym) = ctx.symbols.get(&raw_name) else {
            continue;
        };
        dependencies.push(IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Symbol(resource_sym)),
            (
                KfxSymbol::MandatoryDependencies as u64,
                IonValue::List(vec![IonValue::Symbol(raw_sym)]),
            ),
        ]));
    }

    let mut ion_fields = vec![(
        KfxSymbol::ContainerList as u64,
        IonValue::List(vec![container_entry]),
    )];
    if !dependencies.is_empty() {
        ion_fields.push((
            KfxSymbol::EntityDependencies as u64,
            IonValue::List(dependencies),
        ));
    }
    let ion = IonValue::Struct(ion_fields);

    KfxFragment::container_entity_map(ion)
}

/// Detect format symbol from file extension/magic bytes.
///
/// Delegates to the pure `detect_media_format()` utility and maps to KFX symbol.
fn detect_format_symbol(href: &str, data: &[u8]) -> u64 {
    let format = detect_media_format(href, data);
    format_to_kfx_symbol(format)
}

/// Check if a path is a media asset (image, font, etc.)
fn is_media_asset(path: &std::path::Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    matches!(
        ext.to_lowercase().as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "svg" | "webp" | "ttf" | "otf" | "woff" | "woff2"
    )
}

/// Check if a path is a font asset. Fonts are always bundled: `@font-face`
/// matching in `build_font_fragments` reaches them, section image refs do not.
fn is_font_asset(path: &std::path::Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    matches!(
        ext.to_lowercase().as_str(),
        "ttf" | "otf" | "woff" | "woff2"
    )
}

/// Resolve landmarks from the Book's IR into `landmark_fragments`, taking both
/// chapter-level targets (`chapter.xhtml`) and anchor-level ones
/// (`chapter.xhtml#section1`) through ResolvedLinks.
fn resolve_landmarks_from_ir(
    book: &Book,
    source_to_chapter: &HashMap<String, ChapterId>,
    resolved: &ResolvedLinks,
    ctx: &mut ExportContext,
) {
    for landmark in book.landmarks() {
        // Split href into file path and optional anchor
        let (href_path, _anchor) = match landmark.href.split_once('#') {
            Some((path, anchor)) => (path, Some(anchor)),
            None => (landmark.href.as_str(), None),
        };

        // Try to find the chapter ID for this href
        let chapter_id = source_to_chapter.get(href_path).copied();

        if let Some(cid) = chapter_id {
            // The landmark's href through the book's navigation resolver
            let resolved_target = book.resolve_toc_href(cid, &landmark.href);

            let target = match resolved_target {
                Some(AnchorTarget::Internal(gid)) => {
                    // Look up position for the internal target
                    ctx.position_map
                        .get(&(gid.chapter, gid.node))
                        .map(|pos| LandmarkTarget {
                            fragment_id: pos.fragment_id,
                            offset: 0,
                            label: landmark.label.clone(),
                        })
                }
                Some(AnchorTarget::Chapter(target_chapter)) => {
                    // Use chapter's fragment ID
                    ctx.chapter_fragments
                        .get(&target_chapter)
                        .copied()
                        .map(|frag_id| LandmarkTarget {
                            fragment_id: frag_id,
                            offset: 0,
                            label: landmark.label.clone(),
                        })
                }
                _ => {
                    // Fall back to chapter's fragment ID
                    ctx.chapter_fragments
                        .get(&cid)
                        .copied()
                        .map(|frag_id| LandmarkTarget {
                            fragment_id: frag_id,
                            offset: 0,
                            label: landmark.label.clone(),
                        })
                }
            };

            if let Some(target) = target {
                // Add only on a vacant entry (first wins)
                ctx.landmark_fragments
                    .entry(landmark.landmark_type)
                    .or_insert(target.clone());

                // BodyMatter can serve as StartReading if no explicit SRL
                if landmark.landmark_type == LandmarkType::BodyMatter {
                    ctx.landmark_fragments
                        .entry(LandmarkType::StartReading)
                        .or_insert(target);
                }
            }
        }
    }

    // Suppress unused variable warning - resolved is used for consistency
    let _ = resolved;
}

/// Serialize fragments to entities.
fn serialize_fragments(
    fragments: &[KfxFragment],
    local_symbols: &[String],
) -> Vec<SerializedEntity> {
    fragments
        .iter()
        .map(|frag| {
            let id = if frag.is_singleton() {
                KfxSymbol::Null as u32 // Singleton marker ($348 = null)
            } else {
                // Look up local symbol ID
                local_symbols
                    .iter()
                    .position(|s| s == &frag.fid)
                    .map(|i| (crate::formats::kfx::symbols::KFX_SYMBOL_TABLE_SIZE + i) as u32)
                    .unwrap_or(0)
            };

            let data = match &frag.data {
                crate::formats::kfx::fragment::FragmentData::Ion(value) => {
                    create_entity_data(value)
                }
                crate::formats::kfx::fragment::FragmentData::Raw(bytes) => {
                    crate::formats::kfx::serialization::create_raw_media_data(bytes)
                }
            };

            SerializedEntity {
                id,
                entity_type: frag.ftype as u32,
                data,
            }
        })
        .collect()
}

// Fixed-layout image manga → KFX (yj_non_pdf_fixed_layout): a cover section
// (page box + scale_fit + float:center, bare image storyline), then one section
// per 1–2 facing pages. See [`is_fixed_layout_image_book`].

/// Downscaled page-thumbnail box (Amazon uses ~270×384 for a portrait manga
/// page; `yj_thumbnails_present`). Aspect is preserved within this box.
const MANGA_THUMB_W: u32 = 270;
const MANGA_THUMB_H: u32 = 384;

/// True when the book is emitted as a fixed-layout image manga: the metadata
/// marks it fixed-layout AND every spine page is a single image. Anything else
/// falls through to `build_kfx_container`.
fn is_fixed_layout_image_book(book: &mut Book) -> bool {
    if !book.metadata().fixed_layout {
        return false;
    }
    let ids: Vec<_> = book.spine().iter().map(|e| e.id).collect();
    if ids.is_empty() {
        return false;
    }
    ids.iter().all(|&id| {
        book.load_chapter(id)
            .ok()
            .and_then(|c| get_chapter_image_path(&c))
            .is_some()
    })
}

/// One page image in a fixed-layout manga: its resource symbols (full + optional
/// thumbnail), the index of its encoded bytes in the survey's `encs`, and the
/// content EIDs it occupies in its storyline.
struct MangaPage {
    res_name: String,
    res_sym: u64,
    thumb_name: String,
    /// Thumbnail resource-name symbol; `0` when this page has no thumbnail.
    thumb_sym: u64,
    /// Index into the survey's encoded-bytes vector (`encs`).
    enc: usize,
    /// Nested content EIDs. The cover uses only `image_id` (a bare image); every
    /// other page uses the outer→inner→image trio.
    outer_id: u64,
    inner_id: u64,
    image_id: u64,
}

/// One reading unit = one section + one storyline: a solo page (the cover, or
/// every page of a landscape-canvas book) or a facing spread of 1–2 pages.
struct MangaUnit {
    section_name: String,
    section_sym: u64,
    story_sym: u64,
    /// EID of the section's single page_template container.
    pt_id: u64,
    /// `true` → fixed-box page_template + bare image; `false` → page_spread.
    solo: bool,
    pages: Vec<MangaPage>,
}

/// Encoded bytes + dimensions for one page (full image and its thumbnail),
/// gathered in the survey pass. `img`/`thumb` are moved into `bcRawMedia`
/// fragments during synthesis.
struct MangaEnc {
    img: Vec<u8>,
    w: u32,
    h: u32,
    thumb: Vec<u8>,
    tw: u32,
    th: u32,
}

/// Convert a fixed-layout image book (manga/comic) into a `yj_non_pdf_fixed_layout`
/// KFX. See the module section comment above for the emitted structure. The
/// caller confirms [`is_fixed_layout_image_book`] first.
fn image_fxl_to_kfx(
    book: &mut Book,
    on_progress: &dyn Fn(&str, usize, usize, &str),
) -> io::Result<Vec<u8>> {
    let container_id = generate_container_id(&book.metadata().title);
    let mut ctx = ExportContext::new();

    // Book-level layout signals: a manga is horizontal_tb text with an rtl page
    // turn, `document_direction` resolving rtl from the spine's
    // page-progression-direction. Both reach document_data and the reading order.
    let writing_mode = book_writing_mode(book);
    let direction = document_direction(book, writing_mode);
    ctx.document_writing_mode = writing_mode;
    // FXL/image path: no reflowable text styles override, and the per-style
    // baseline tracks the document mode.
    ctx.style_writing_mode_baseline = writing_mode;
    ctx.document_direction = direction;
    let ppd_sym = Some(direction);

    let color_mode = book.image_color_mode();
    let language = book.metadata().language.clone();
    let mut box_dims = book.metadata().default_viewport;

    // ---- Load the per-page image href for every spine page ----
    let mut spine_ids: Vec<_> = book.spine().iter().map(|e| e.id).collect();
    // The source's declared facing-page pairing, one entry per spine page.
    let mut sides: Vec<Option<crate::model::PageSpread>> =
        book.spine().iter().map(|e| e.page_spread).collect();
    let mut viewports: Vec<Option<(u32, u32)>> = book.spine().iter().map(|e| e.viewport).collect();
    let mut hrefs: Vec<String> = Vec::with_capacity(spine_ids.len());
    for &id in &spine_ids {
        let chapter = book.load_chapter(id)?;
        let href = get_chapter_image_path(&chapter).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "fixed-layout page is not a single image",
            )
        })?;
        hrefs.push(href);
    }

    // Resolve each IR image href against the asset list (exact path, else by
    // file name): an `images/x.jpg` src finds its bytes either way.
    let assets: Vec<std::path::PathBuf> = book.list_assets().to_vec();
    let resolve = |href: &str| -> Option<std::path::PathBuf> {
        let want = std::path::Path::new(href);
        if assets.iter().any(|a| a.as_path() == want) {
            return Some(want.to_path_buf());
        }
        let fname = want.file_name()?;
        assets
            .iter()
            .find(|a| a.file_name() == Some(fname))
            .cloned()
    };

    // Drop calibre's dual cover: its synthetic title page (spine[0]) reuses the
    // image of the first content page (spine[1]), detected by identical resolved
    // paths. The real first page becomes the solo cover.
    if spine_ids.len() >= 2
        && let (Some(a), Some(b)) = (resolve(&hrefs[0]), resolve(&hrefs[1]))
        && a == b
    {
        spine_ids.remove(0);
        hrefs.remove(0);
        sides.remove(0);
        viewports.remove(0);
    }
    sides.resize(hrefs.len(), None);
    viewports.resize(hrefs.len(), None);

    // Fallback page box when no viewport was declared: the largest page image.
    if box_dims.is_none() {
        let mut mx = 0;
        let mut my = 0;
        for href in &hrefs {
            let Some((w, h)) = resolve(href)
                .and_then(|p| book.load_asset(&p).ok())
                .and_then(|raw| crate::util::extract_image_dimensions(&raw))
            else {
                continue;
            };
            mx = mx.max(w);
            my = my.max(h);
        }
        if mx > 0 && my > 0 {
            box_dims = Some((mx, my));
        }
    }
    let (box_w, box_h) = box_dims.unwrap_or((1, 1));
    // Facing-page pairing applies to a portrait canvas only.
    let portrait_canvas = box_h > box_w;

    // ---- Encode pages, splitting a page that is itself a facing spread ----
    // A wide page image in a portrait-canvas book carries one spread; its halves
    // fill the two slots of one `page_spread` section.
    let mut encs: Vec<MangaEnc> = Vec::with_capacity(hrefs.len());
    // Emitted page → (source spine index, declared side).
    let mut origin: Vec<(usize, Option<crate::model::PageSpread>)> =
        Vec::with_capacity(hrefs.len());
    let total = hrefs.len();
    for (i, href) in hrefs.iter().enumerate() {
        on_progress("images", i + 1, total, "Encoding pages");
        let path = resolve(href).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("fixed-layout page image not found: {href}"),
            )
        })?;
        let raw = book.load_asset(&path)?;
        reject_unrasterizable_svg(href, &raw)?;
        let (w, h) = crate::util::extract_image_dimensions(&raw).unwrap_or((0, 0));
        let split =
            i > 0 && portrait_canvas && is_facing_spread_image(w, h, box_w, box_h, viewports[i]);
        if split && let Some(halves) = split_spread_image(&raw, color_mode, direction) {
            let (first, second) = halves;
            encs.push(first);
            origin.push((i, Some(crate::model::PageSpread::Left)));
            encs.push(second);
            origin.push((i, Some(crate::model::PageSpread::Right)));
            continue;
        }
        let img = encode_asset_for_kfx(&raw, color_mode);
        let (thumb, tw, th) = make_manga_thumbnail(&raw, color_mode).unwrap_or((Vec::new(), 0, 0));
        encs.push(MangaEnc {
            img,
            w,
            h,
            thumb,
            tw,
            th,
        });
        origin.push((i, sides[i]));
    }
    let n = encs.len();

    // The declared cover, resolved by `build_book_metadata_fragment` for the
    // library tile. A cover that is page 0 shares that page's `e0`; one the
    // spine never shows becomes a resource of its own.
    let cover_href = book.metadata().cover_image.clone();
    let cover_is_page0 = match (&cover_href, hrefs.first()) {
        (Some(c), Some(p0)) => resolve(c) == resolve(p0),
        _ => false,
    };
    if cover_is_page0 {
        let href = hrefs[0].clone();
        ctx.resource_registry.register(&href, &mut ctx.symbols);
        let _ = ctx.resource_registry.get_or_create_name(&href); // -> "e0"
    }
    // Bytes for a cover the spine never shows, encoded once and emitted beside
    // the page resources under `MANGA_COVER_RSRC`.
    let standalone_cover: Option<MangaEnc> = (!cover_is_page0)
        .then_some(cover_href.as_deref())
        .flatten()
        .and_then(|c| Some((c.to_string(), resolve(c)?)))
        .and_then(|(c, path)| {
            let raw = book.load_asset(&path).ok()?;
            let (w, h) = crate::util::extract_image_dimensions(&raw)?;
            ctx.resource_registry.assign_name(&c, MANGA_COVER_RSRC);
            ctx.symbols.get_or_intern(MANGA_COVER_RSRC);
            ctx.symbols
                .get_or_intern(&format!("resource/{MANGA_COVER_RSRC}"));
            ctx.symbols.get_or_intern(MANGA_COVER_THUMB);
            ctx.symbols
                .get_or_intern(&format!("resource/{MANGA_COVER_THUMB}"));
            let (thumb, tw, th) = make_cover_thumbnail(&raw).unwrap_or((Vec::new(), 0, 0));
            Some(MangaEnc {
                img: cover_jpeg_for_kfx(&raw).unwrap_or(raw.clone()),
                w,
                h,
                thumb,
                tw,
                th,
            })
        });

    // The cover resource a fixed-layout book names in its `$258 metadata`:
    // `MANGA_COVER_RSRC` for a cover the spine never shows, page 0's `e0`.
    let cover_resource_sym = if standalone_cover.is_some() {
        Some(ctx.symbols.get_or_intern(MANGA_COVER_RSRC))
    } else if cover_is_page0 {
        Some(ctx.symbols.get_or_intern("e0"))
    } else {
        None
    };

    // Shared image styles (the reference KFX's `sJ` inner-container fill and
    // `sG` page-box image placement), interned once up front.
    let sj_sym = ctx.symbols.get_or_intern("sJ");
    let sg_sym = ctx.symbols.get_or_intern("sG");

    // ---- Group pages into units, allocate section / story / EIDs ----
    let groups = manga_page_groups(&origin, portrait_canvas);

    // Emitted page index → its image EID (nav targets) and spine ChapterId → page
    // index (TOC target resolution), filled as units are built.
    let mut page_image_id = vec![0u64; n];
    // A split spread emits two pages from one spine entry; the TOC lands on the
    // first of them.
    let mut chapter_to_index: std::collections::HashMap<ChapterId, usize> =
        std::collections::HashMap::new();
    for (emitted, &(src, _)) in origin.iter().enumerate() {
        if let Some(&id) = spine_ids.get(src) {
            chapter_to_index.entry(id).or_insert(emitted);
        }
    }

    let mut units: Vec<MangaUnit> = Vec::with_capacity(groups.len());
    let mut any_spread = false;
    let mut any_thumb = false;
    for (u, group) in groups.iter().enumerate() {
        let section_name = format!("c{u}");
        let section_sym = ctx.register_section(&section_name);
        let story_sym = ctx.symbols.get_or_intern(&format!("story_c{u}"));
        // The cover opens alone; on a landscape canvas every page does.
        let solo = u == 0 || !portrait_canvas;
        let pt_id = ctx.next_fragment_id();
        if group.len() > 1 {
            any_spread = true;
        }
        let mut pages = Vec::with_capacity(group.len());
        for &pi in group {
            let res_name = format!("e{pi}");
            let res_sym = ctx.symbols.get_or_intern(&res_name);
            ctx.symbols.get_or_intern(&format!("resource/{res_name}"));
            ctx.record_section_image_ref(&section_name, &res_name);
            let (thumb_name, thumb_sym) = if encs[pi].thumb.is_empty() {
                (String::new(), 0)
            } else {
                any_thumb = true;
                let tn = format!("e{pi}-thumb");
                let ts = ctx.symbols.get_or_intern(&tn);
                ctx.symbols.get_or_intern(&format!("resource/{tn}"));
                (tn, ts)
            };
            // EIDs: solo = bare image; facing = outer→inner→image.
            let (outer_id, inner_id, image_id) = if solo {
                (0, 0, ctx.next_fragment_id())
            } else {
                let o = ctx.next_fragment_id();
                let inr = ctx.next_fragment_id();
                let im = ctx.next_fragment_id();
                (o, inr, im)
            };
            ctx.record_content_length(image_id, 1);
            page_image_id[pi] = image_id;
            pages.push(MangaPage {
                res_name,
                res_sym,
                thumb_name,
                thumb_sym,
                enc: pi,
                outer_id,
                inner_id,
                image_id,
            });
        }
        units.push(MangaUnit {
            section_name,
            section_sym,
            story_sym,
            pt_id,
            solo,
            pages,
        });
    }

    // ---- Synthesis (reference entity order) ----
    let mut fragments: Vec<KfxFragment> = Vec::new();

    // 1. content_features ($585)
    fragments.push(build_manga_content_features_fragment(any_spread, any_thumb));
    // 2. book_metadata ($490) — reuse the reflowable builder (reads Book metadata)
    ctx.has_publisher_fonts = has_publisher_fonts(book);
    ctx.fixed_layout_book = true;
    ctx.double_page_spread = any_spread;
    fragments.push(build_book_metadata_fragment(book, &container_id, &ctx));
    // 3. metadata ($258) — reading order + page-progression-direction
    fragments.push(build_fxl_metadata_fragment(
        &ctx,
        ppd_sym,
        cover_resource_sym,
    ));
    // 4. document_data ($538) — inserted here later, once max_id is known.
    let document_data_index = fragments.len();

    // 5. sections ($260) + 6. storylines ($259). A solo page_template is sized to
    // its own image (shown full-screen, no letterbox); facing pages share the
    // uniform page box and align.
    let mut sections = Vec::with_capacity(units.len());
    let mut storylines = Vec::with_capacity(units.len());
    for unit in &units {
        let solo_dims = unit
            .solo
            .then(|| unit.pages.first())
            .flatten()
            .map(|p| &encs[p.enc])
            .filter(|e| e.w > 0 && e.h > 0)
            .map(|e| (e.w, e.h));
        let (sw, sh) = solo_dims.unwrap_or((box_w, box_h));
        sections.push(build_manga_section(unit, sw, sh));
        storylines.push(build_manga_storyline(unit, box_w, box_h, sj_sym, sg_sym));
    }
    fragments.extend(sections);
    fragments.extend(storylines);

    // 7. styles ($157) — sJ inner fill + sG page-box image placement
    fragments.push(build_manga_style_sj(sj_sym, &language));
    fragments.push(build_manga_style_sg(sg_sym, box_w, box_h));

    // 8. external_resource ($164) — full images (with `thumbnails` link) then
    // thumbs, plus a cover the spine never shows, which belongs to no section.
    if let Some(MangaEnc {
        img: bytes,
        w,
        h,
        thumb,
        tw,
        th,
    }) = &standalone_cover
    {
        let sym = ctx.symbols.get_or_intern(MANGA_COVER_RSRC);
        let fmt_sym = detect_format_symbol(MANGA_COVER_RSRC, bytes);
        let mut fields = vec![
            (KfxSymbol::ResourceName as u64, IonValue::Symbol(sym)),
            (
                KfxSymbol::Location as u64,
                IonValue::String(format!("resource/{MANGA_COVER_RSRC}")),
            ),
            (KfxSymbol::Format as u64, IonValue::Symbol(fmt_sym)),
            (KfxSymbol::ResourceWidth as u64, IonValue::Int(*w as i64)),
            (KfxSymbol::ResourceHeight as u64, IonValue::Int(*h as i64)),
        ];
        if !thumb.is_empty() {
            let tsym = ctx.symbols.get_or_intern(MANGA_COVER_THUMB);
            fields.push((KfxSymbol::Thumbnails as u64, IonValue::Symbol(tsym)));
        }
        if let Some(m) = manga_format_mime(fmt_sym) {
            fields.push((KfxSymbol::Mime as u64, IonValue::String(m.to_string())));
        }
        fragments.push(KfxFragment::new(
            KfxSymbol::ExternalResource,
            MANGA_COVER_RSRC,
            IonValue::Struct(fields),
        ));
        if !thumb.is_empty() {
            let tsym = ctx.symbols.get_or_intern(MANGA_COVER_THUMB);
            let tfmt = detect_format_symbol(MANGA_COVER_THUMB, thumb);
            let mut tfields = vec![
                (KfxSymbol::ResourceName as u64, IonValue::Symbol(tsym)),
                (
                    KfxSymbol::Location as u64,
                    IonValue::String(format!("resource/{MANGA_COVER_THUMB}")),
                ),
                (KfxSymbol::Format as u64, IonValue::Symbol(tfmt)),
                (KfxSymbol::ResourceWidth as u64, IonValue::Int(*tw as i64)),
                (KfxSymbol::ResourceHeight as u64, IonValue::Int(*th as i64)),
            ];
            if let Some(m) = manga_format_mime(tfmt) {
                tfields.push((KfxSymbol::Mime as u64, IonValue::String(m.to_string())));
            }
            fragments.push(KfxFragment::new(
                KfxSymbol::ExternalResource,
                MANGA_COVER_THUMB,
                IonValue::Struct(tfields),
            ));
        }
    }
    for unit in &units {
        for p in &unit.pages {
            let e = &encs[p.enc];
            let fmt_sym = detect_format_symbol(&p.res_name, &e.img);
            fragments.push(build_manga_external_resource(p, e.w, e.h, fmt_sym));
            if p.thumb_sym != 0 {
                let tfmt = detect_format_symbol(&p.thumb_name, &e.thumb);
                fragments.push(build_manga_thumb_external_resource(p, e.tw, e.th, tfmt));
            }
        }
    }
    // 9. bcRawMedia ($417) — the actual bytes (moved out of `encs`)
    if let Some(MangaEnc {
        img: bytes, thumb, ..
    }) = standalone_cover
    {
        fragments.push(KfxFragment::raw(
            KfxSymbol::Bcrawmedia as u64,
            format!("resource/{MANGA_COVER_RSRC}"),
            bytes,
        ));
        if !thumb.is_empty() {
            fragments.push(KfxFragment::raw(
                KfxSymbol::Bcrawmedia as u64,
                format!("resource/{MANGA_COVER_THUMB}"),
                thumb,
            ));
        }
    }
    for unit in &units {
        for p in &unit.pages {
            let img = std::mem::take(&mut encs[p.enc].img);
            fragments.push(KfxFragment::raw(
                KfxSymbol::Bcrawmedia as u64,
                format!("resource/{}", p.res_name),
                img,
            ));
            if p.thumb_sym != 0 {
                let thumb = std::mem::take(&mut encs[p.enc].thumb);
                fragments.push(KfxFragment::raw(
                    KfxSymbol::Bcrawmedia as u64,
                    format!("resource/{}", p.thumb_name),
                    thumb,
                ));
            }
        }
    }

    // 10. navigation — referenced nav_containers (`landmarks` cover_page → the
    // cover's page_template, `toc` mapping the source TOC to page image EIDs)
    // plus a thin book_navigation. A failure yields a landmarks-only nav.
    let _ = book.resolve_links();
    let toc = book.toc().to_vec();
    fragments.extend(build_manga_nav_fragments(
        &units,
        &toc,
        &page_image_id,
        &chapter_to_index,
        &mut ctx,
    ));

    // 11. position system — section-keyed; nav targets resolve on-device.
    fragments.push(build_manga_position_map_fragment(&units));
    fragments.push(build_manga_position_id_map_fragment(&units));
    fragments.extend(build_manga_section_position_id_map_fragments(&units));

    // 12. container metadata
    fragments.push(build_resource_path_fragment());
    fragments.push(build_manga_container_entity_map_fragment(
        &container_id,
        &fragments,
        &units,
        &ctx,
    ));

    // document_data, with every EID allocated (max_id correct).
    fragments.insert(
        document_data_index,
        build_manga_document_data_fragment(&ctx, ppd_sym),
    );

    // ---- Serialize ----
    let symtab_ion = build_symbol_table_ion(ctx.symbols.local_symbols());
    let format_caps_ion = build_format_capabilities_ion();
    let entities = serialize_fragments(&fragments, ctx.symbols.local_symbols());
    on_progress("finalize", 1, 1, "Finalizing");
    Ok(serialize_container(
        &container_id,
        &entities,
        &symtab_ion,
        &format_caps_ion,
    ))
}

/// Partition emitted pages into reading units: page 0 solo, then facing pairs
/// keyed on the declared `page-spread-left` / `-right`, consecutive where none
/// is declared, every page solo on a landscape canvas. Groups are page indices.
fn manga_page_groups(
    origin: &[(usize, Option<crate::model::PageSpread>)],
    portrait_canvas: bool,
) -> Vec<Vec<usize>> {
    use crate::model::PageSpread;
    let n = origin.len();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    if n == 0 {
        return groups;
    }
    if !portrait_canvas {
        return (0..n).map(|i| vec![i]).collect();
    }
    groups.push(vec![0]);
    let mut i = 1;
    while i < n {
        let pairs = match (origin[i].1, origin.get(i + 1).and_then(|o| o.1)) {
            (Some(PageSpread::Left), Some(PageSpread::Right)) => true,
            (Some(_), _) => false,
            (None, Some(_)) => false,
            (None, _) => i + 1 < n,
        };
        if pairs {
            groups.push(vec![i, i + 1]);
            i += 2;
        } else {
            groups.push(vec![i]);
            i += 1;
        }
    }
    groups
}

/// Resource name for a cover the spine never shows.
const MANGA_COVER_RSRC: &str = "ecover";

/// Resource name for that cover's downscaled thumbnail.
const MANGA_COVER_THUMB: &str = "ecover-thumb";

/// True when a page image carries one facing spread: at least 1.5× as wide,
/// against the page box, as a single page. A `viewport` half the canvas height
/// states the same shape.
fn is_facing_spread_image(
    w: u32,
    h: u32,
    box_w: u32,
    box_h: u32,
    viewport: Option<(u32, u32)>,
) -> bool {
    if w == 0 || h == 0 || box_w == 0 || box_h == 0 {
        return false;
    }
    if let Some((vw, vh)) = viewport
        && vh > 0
        && u64::from(vw) * u64::from(box_h) >= u64::from(vh) * u64::from(box_w) * 3 / 2
    {
        return true;
    }
    u64::from(w) * u64::from(box_h) * 2 >= u64::from(h) * u64::from(box_w) * 3
}

/// Cut a facing-spread image down its middle into two page images in reading
/// order: the right half leads an rtl book, the left half an ltr one. `None`
/// when `raw` doesn't decode.
fn split_spread_image(
    raw: &[u8],
    mode: jxr::ColorMode,
    direction: KfxSymbol,
) -> Option<(MangaEnc, MangaEnc)> {
    let img = ::image::load_from_memory(raw).ok()?;
    let (w, h) = (img.width(), img.height());
    if w < 2 || h == 0 {
        return None;
    }
    let mid = w / 2;
    let build = |x: u32, cw: u32| -> Option<MangaEnc> {
        let half = img.crop_imm(x, 0, cw, h);
        let (thumb, tw, th) = manga_thumbnail_of(&half, mode).unwrap_or((Vec::new(), 0, 0));
        Some(MangaEnc {
            img: encode_dynimg_jxr(&half, mode)?,
            w: cw,
            h,
            thumb,
            tw,
            th,
        })
    };
    let left = build(0, mid)?;
    let right = build(mid, w - mid)?;
    Some(if direction == KfxSymbol::Rtl {
        (right, left)
    } else {
        (left, right)
    })
}

/// Downscale a page image to a thumbnail (aspect preserved, within
/// [`MANGA_THUMB_W`]×[`MANGA_THUMB_H`]) and JXR-encode it. `None` for an
/// undecodable raster or a rejected encode; the page ships without one.
fn make_manga_thumbnail(data: &[u8], mode: jxr::ColorMode) -> Option<(Vec<u8>, u32, u32)> {
    manga_thumbnail_of(&::image::load_from_memory(data).ok()?, mode)
}

/// A cover's downscaled thumbnail, JPEG-encoded like the cover itself. The
/// library-gallery and sleep-screen thumbnailer reads JPEG only.
fn make_cover_thumbnail(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let img = ::image::load_from_memory(data).ok()?;
    let thumb = img.thumbnail(MANGA_THUMB_W, MANGA_THUMB_H);
    let (tw, th) = (thumb.width(), thumb.height());
    Some((crate::image::jpeg::encode_as_jpeg(&thumb)?, tw, th))
}

/// [`make_manga_thumbnail`] over a decoded image.
fn manga_thumbnail_of(
    img: &::image::DynamicImage,
    mode: jxr::ColorMode,
) -> Option<(Vec<u8>, u32, u32)> {
    let thumb = img.thumbnail(MANGA_THUMB_W, MANGA_THUMB_H);
    let (tw, th) = (thumb.width(), thumb.height());
    let bytes = encode_dynimg_jxr(&thumb, mode)?;
    Some((bytes, tw, th))
}

/// A KFX `content_features` feature struct `{namespace, key, version_info}`.
fn manga_feature(namespace: &str, key: &str, major: i64) -> IonValue {
    IonValue::Struct(vec![
        (
            KfxSymbol::Namespace as u64,
            IonValue::String(namespace.to_string()),
        ),
        (KfxSymbol::Key as u64, IonValue::String(key.to_string())),
        (
            KfxSymbol::VersionInfo as u64,
            IonValue::Struct(vec![(
                KfxSymbol::Version as u64,
                IonValue::Struct(vec![
                    (KfxSymbol::MajorVersion as u64, IonValue::Int(major)),
                    (KfxSymbol::MinorVersion as u64, IonValue::Int(0)),
                ]),
            )]),
        ),
    ])
}

/// content_features ($585) for a fixed-layout image manga: `CanonicalFormat` +
/// `yj_non_pdf_fixed_layout` (v2), plus `yj_double_page_spread` for a real 2-page
/// spread and `yj_thumbnails_present` for a page thumbnail.
fn build_manga_content_features_fragment(any_spread: bool, any_thumb: bool) -> KfxFragment {
    const YJ: &str = "com.amazon.yjconversion";
    let mut feats = vec![manga_feature("SDK.Marker", "CanonicalFormat", 2)];
    if any_spread {
        feats.push(manga_feature(YJ, "yj_double_page_spread", 1));
    }
    feats.push(manga_feature(YJ, "yj_non_pdf_fixed_layout", 2));
    if any_thumb {
        feats.push(manga_feature(YJ, "yj_thumbnails_present", 1));
    }
    let ion = IonValue::Struct(vec![(KfxSymbol::Features as u64, IonValue::List(feats))]);
    KfxFragment::singleton(KfxSymbol::ContentFeatures, ion)
}

/// document_data ($538) for a fixed-layout image manga: the book-level
/// writing_mode + direction (the device's page-turn signal), max_id,
/// spacing_percent_base, and the reading order.
fn build_manga_document_data_fragment(
    ctx: &ExportContext,
    ppd_sym: Option<KfxSymbol>,
) -> KfxFragment {
    let reading_order = default_reading_order(ctx, ppd_sym);
    let fields = vec![
        (
            KfxSymbol::WritingMode as u64,
            IonValue::Symbol(ctx.document_writing_mode as u64),
        ),
        (
            KfxSymbol::Direction as u64,
            IonValue::Symbol(ctx.document_direction as u64),
        ),
        (KfxSymbol::MaxId as u64, IonValue::Int(ctx.max_eid() as i64)),
        (
            KfxSymbol::SpacingPercentBase as u64,
            IonValue::Symbol(KfxSymbol::Width as u64),
        ),
        (
            KfxSymbol::ReadingOrders as u64,
            IonValue::List(vec![reading_order]),
        ),
    ];
    KfxFragment::singleton(KfxSymbol::DocumentData, IonValue::Struct(fields))
}

/// section ($260) for a manga unit. A solo page_template carries the page box +
/// scale_fit; a facing one is `layout:page_spread`, its dimensions held by the
/// storyline's page containers. Both enable `virtual_panel` (Kindle Panel View).
fn build_manga_section(unit: &MangaUnit, box_w: u32, box_h: u32) -> KfxFragment {
    let template = if unit.solo {
        IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Int(unit.pt_id as i64)),
            (
                KfxSymbol::StoryName as u64,
                IonValue::Symbol(unit.story_sym),
            ),
            (KfxSymbol::FixedWidth as u64, IonValue::Int(box_w as i64)),
            (KfxSymbol::FixedHeight as u64, IonValue::Int(box_h as i64)),
            (
                KfxSymbol::Layout as u64,
                IonValue::Symbol(KfxSymbol::ScaleFit as u64),
            ),
            (
                KfxSymbol::Float as u64,
                IonValue::Symbol(KfxSymbol::Center as u64),
            ),
            (
                KfxSymbol::VirtualPanel as u64,
                IonValue::Symbol(KfxSymbol::Enabled as u64),
            ),
            (
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::Container as u64),
            ),
        ])
    } else {
        IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Int(unit.pt_id as i64)),
            (
                KfxSymbol::StoryName as u64,
                IonValue::Symbol(unit.story_sym),
            ),
            (
                KfxSymbol::VirtualPanel as u64,
                IonValue::Symbol(KfxSymbol::Enabled as u64),
            ),
            (
                KfxSymbol::Layout as u64,
                IonValue::Symbol(KfxSymbol::PageSpread as u64),
            ),
            (
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::Container as u64),
            ),
        ])
    };
    let ion = IonValue::Struct(vec![
        (
            KfxSymbol::SectionName as u64,
            IonValue::Symbol(unit.section_sym),
        ),
        (
            KfxSymbol::PageTemplates as u64,
            IonValue::List(vec![template]),
        ),
    ]);
    KfxFragment::new(KfxSymbol::Section, &unit.section_name, ion)
}

/// storyline ($259) for a manga unit: a solo unit's is the bare image; a facing
/// unit's holds one scale_fit/float:center page container per page, each an inner
/// vertical container (`sJ`) around the page image (`sG`).
fn build_manga_storyline(
    unit: &MangaUnit,
    box_w: u32,
    box_h: u32,
    sj_sym: u64,
    sg_sym: u64,
) -> KfxFragment {
    let content: Vec<IonValue> = if unit.solo {
        unit.pages
            .iter()
            .map(|p| {
                IonValue::Struct(vec![
                    (KfxSymbol::Id as u64, IonValue::Int(p.image_id as i64)),
                    (
                        KfxSymbol::Type as u64,
                        IonValue::Symbol(KfxSymbol::Image as u64),
                    ),
                    (KfxSymbol::ResourceName as u64, IonValue::Symbol(p.res_sym)),
                ])
            })
            .collect()
    } else {
        unit.pages
            .iter()
            .map(|p| {
                let image = IonValue::Struct(vec![
                    (KfxSymbol::Id as u64, IonValue::Int(p.image_id as i64)),
                    (
                        KfxSymbol::Type as u64,
                        IonValue::Symbol(KfxSymbol::Image as u64),
                    ),
                    (KfxSymbol::Style as u64, IonValue::Symbol(sg_sym)),
                    (KfxSymbol::ResourceName as u64, IonValue::Symbol(p.res_sym)),
                ]);
                let inner = IonValue::Struct(vec![
                    (KfxSymbol::Id as u64, IonValue::Int(p.inner_id as i64)),
                    (
                        KfxSymbol::Layout as u64,
                        IonValue::Symbol(KfxSymbol::Vertical as u64),
                    ),
                    (KfxSymbol::Style as u64, IonValue::Symbol(sj_sym)),
                    (
                        KfxSymbol::Type as u64,
                        IonValue::Symbol(KfxSymbol::Container as u64),
                    ),
                    (KfxSymbol::ContentList as u64, IonValue::List(vec![image])),
                ]);
                IonValue::Struct(vec![
                    (KfxSymbol::Id as u64, IonValue::Int(p.outer_id as i64)),
                    (
                        KfxSymbol::WritingMode as u64,
                        IonValue::Symbol(KfxSymbol::HorizontalTb as u64),
                    ),
                    (
                        KfxSymbol::Direction as u64,
                        IonValue::Symbol(KfxSymbol::Ltr as u64),
                    ),
                    (KfxSymbol::FontSize as u64, IonValue::Int(16)),
                    (KfxSymbol::FixedWidth as u64, IonValue::Int(box_w as i64)),
                    (KfxSymbol::FixedHeight as u64, IonValue::Int(box_h as i64)),
                    (
                        KfxSymbol::Layout as u64,
                        IonValue::Symbol(KfxSymbol::ScaleFit as u64),
                    ),
                    (
                        KfxSymbol::Float as u64,
                        IonValue::Symbol(KfxSymbol::Center as u64),
                    ),
                    (
                        KfxSymbol::Type as u64,
                        IonValue::Symbol(KfxSymbol::Container as u64),
                    ),
                    (KfxSymbol::ContentList as u64, IonValue::List(vec![inner])),
                ])
            })
            .collect()
    };
    let ion = IonValue::Struct(vec![
        (
            KfxSymbol::StoryName as u64,
            IonValue::Symbol(unit.story_sym),
        ),
        (KfxSymbol::ContentList as u64, IonValue::List(content)),
    ]);
    KfxFragment::new(
        KfxSymbol::Storyline,
        format!("story_{}", unit.section_name),
        ion,
    )
}

/// style ($157) `sJ`: the inner container fill — 100%×100%, tagged with the
/// book language (mirrors the reference manga's shared inner-container style).
fn build_manga_style_sj(sj_sym: u64, language: &str) -> KfxFragment {
    let mut fields = vec![
        (KfxSymbol::Width as u64, percent_100()),
        (KfxSymbol::Height as u64, percent_100()),
    ];
    if !language.is_empty() {
        fields.push((
            KfxSymbol::Language as u64,
            IonValue::String(language.to_string()),
        ));
    }
    fields.push((KfxSymbol::StyleName as u64, IonValue::Symbol(sj_sym)));
    KfxFragment::new(KfxSymbol::Style, "sJ", IonValue::Struct(fields))
}

/// style ($157) `sG`: the page-box image placement — the uniform page box,
/// top-left origin, centered.
fn build_manga_style_sg(sg_sym: u64, box_w: u32, box_h: u32) -> KfxFragment {
    let ion = IonValue::Struct(vec![
        (KfxSymbol::Width as u64, IonValue::Int(box_w as i64)),
        (KfxSymbol::Height as u64, IonValue::Int(box_h as i64)),
        (KfxSymbol::Top as u64, IonValue::Int(0)),
        (KfxSymbol::Left as u64, IonValue::Int(0)),
        (
            KfxSymbol::BoxAlign as u64,
            IonValue::Symbol(KfxSymbol::Center as u64),
        ),
        (KfxSymbol::StyleName as u64, IonValue::Symbol(sg_sym)),
    ]);
    KfxFragment::new(KfxSymbol::Style, "sG", ion)
}

/// A resource mime string for a KFX image-format symbol (descriptive; the device
/// resolves by the `format` symbol). `None` for formats without a stable mime.
fn manga_format_mime(fmt_sym: u64) -> Option<&'static str> {
    if fmt_sym == KfxSymbol::Jpg as u64 {
        Some("image/jpeg")
    } else if fmt_sym == KfxSymbol::Jxr as u64 {
        Some("image/vnd.ms-photo")
    } else if fmt_sym == KfxSymbol::Png as u64 {
        Some("image/png")
    } else {
        None
    }
}

/// external_resource ($164) for a page's full image, carrying the `thumbnails`
/// link to its thumbnail resource when present.
fn build_manga_external_resource(p: &MangaPage, w: u32, h: u32, fmt_sym: u64) -> KfxFragment {
    let mut fields = vec![
        (KfxSymbol::ResourceName as u64, IonValue::Symbol(p.res_sym)),
        (
            KfxSymbol::Location as u64,
            IonValue::String(format!("resource/{}", p.res_name)),
        ),
        (KfxSymbol::Format as u64, IonValue::Symbol(fmt_sym)),
        (KfxSymbol::ResourceWidth as u64, IonValue::Int(w as i64)),
        (KfxSymbol::ResourceHeight as u64, IonValue::Int(h as i64)),
    ];
    if p.thumb_sym != 0 {
        fields.push((KfxSymbol::Thumbnails as u64, IonValue::Symbol(p.thumb_sym)));
    }
    if let Some(m) = manga_format_mime(fmt_sym) {
        fields.push((KfxSymbol::Mime as u64, IonValue::String(m.to_string())));
    }
    KfxFragment::new(
        KfxSymbol::ExternalResource,
        &p.res_name,
        IonValue::Struct(fields),
    )
}

/// external_resource ($164) for a page's downscaled thumbnail.
fn build_manga_thumb_external_resource(
    p: &MangaPage,
    tw: u32,
    th: u32,
    fmt_sym: u64,
) -> KfxFragment {
    let mut fields = vec![
        (
            KfxSymbol::ResourceName as u64,
            IonValue::Symbol(p.thumb_sym),
        ),
        (
            KfxSymbol::Location as u64,
            IonValue::String(format!("resource/{}", p.thumb_name)),
        ),
        (KfxSymbol::Format as u64, IonValue::Symbol(fmt_sym)),
        (KfxSymbol::ResourceWidth as u64, IonValue::Int(tw as i64)),
        (KfxSymbol::ResourceHeight as u64, IonValue::Int(th as i64)),
    ];
    if let Some(m) = manga_format_mime(fmt_sym) {
        fields.push((KfxSymbol::Mime as u64, IonValue::String(m.to_string())));
    }
    KfxFragment::new(
        KfxSymbol::ExternalResource,
        &p.thumb_name,
        IonValue::Struct(fields),
    )
}

/// Navigation for a fixed-layout manga, in the referenced form: a `landmarks`
/// nav_container (cover_page → the cover's page_template), a `toc` container
/// mapping the source TOC onto page image EIDs, and a thin `book_navigation`.
fn build_manga_nav_fragments(
    units: &[MangaUnit],
    toc: &[crate::model::TocEntry],
    page_image_id: &[u64],
    chapter_to_index: &std::collections::HashMap<ChapterId, usize>,
    ctx: &mut ExportContext,
) -> Vec<KfxFragment> {
    let Some(cover) = units.first() else {
        return Vec::new();
    };
    let mut fragments = Vec::new();
    let mut container_syms = Vec::new();

    // landmarks: cover_page → the cover section's page_template (full-screen).
    let landmark = IonValue::Annotated(
        vec![KfxSymbol::NavUnit as u64],
        Box::new(IonValue::Struct(vec![
            (
                KfxSymbol::LandmarkType as u64,
                IonValue::Symbol(KfxSymbol::CoverPage as u64),
            ),
            (
                KfxSymbol::Representation as u64,
                IonValue::Struct(vec![(
                    KfxSymbol::Label as u64,
                    IonValue::String("cover-nav-unit".to_string()),
                )]),
            ),
            (
                KfxSymbol::TargetPosition as u64,
                IonValue::Struct(vec![
                    (KfxSymbol::Id as u64, IonValue::Int(cover.pt_id as i64)),
                    (KfxSymbol::Offset as u64, IonValue::Int(0)),
                ]),
            ),
        ])),
    );
    let lmk_sym = ctx.symbols.get_or_intern("nlmk");
    fragments.push(KfxFragment::new(
        KfxSymbol::NavContainer,
        "nlmk",
        IonValue::Struct(vec![
            (
                KfxSymbol::NavType as u64,
                IonValue::Symbol(KfxSymbol::Landmarks as u64),
            ),
            (
                KfxSymbol::NavContainerName as u64,
                IonValue::Symbol(lmk_sym),
            ),
            (KfxSymbol::Entries as u64, IonValue::List(vec![landmark])),
        ]),
    ));
    container_syms.push(lmk_sym);

    // toc: the source table of contents, each entry retargeted to its page's
    // image EID, which `position_id_map` registers.
    let toc_entries = build_manga_toc_entries(toc, page_image_id, chapter_to_index);
    if !toc_entries.is_empty() {
        let toc_sym = ctx.symbols.get_or_intern("ntoc");
        fragments.push(KfxFragment::new(
            KfxSymbol::NavContainer,
            "ntoc",
            IonValue::Struct(vec![
                (
                    KfxSymbol::NavType as u64,
                    IonValue::Symbol(KfxSymbol::Toc as u64),
                ),
                (
                    KfxSymbol::NavContainerName as u64,
                    IonValue::Symbol(toc_sym),
                ),
                (KfxSymbol::Entries as u64, IonValue::List(toc_entries)),
            ]),
        ));
        container_syms.push(toc_sym);
    }

    fragments.push(KfxFragment::singleton(
        KfxSymbol::BookNavigation,
        IonValue::List(vec![IonValue::Struct(vec![
            (
                KfxSymbol::ReadingOrderName as u64,
                IonValue::Symbol(KfxSymbol::Default as u64),
            ),
            (
                KfxSymbol::NavContainers as u64,
                IonValue::List(container_syms.into_iter().map(IonValue::Symbol).collect()),
            ),
        ])]),
    ));
    fragments
}

/// Resolve a source TOC entry's target to the spine page index it lands on
/// (either a chapter start or an intra-chapter anchor — both name a page).
fn toc_target_index(
    entry: &crate::model::TocEntry,
    chapter_to_index: &std::collections::HashMap<ChapterId, usize>,
) -> Option<usize> {
    match entry.target.as_ref()? {
        crate::model::AnchorTarget::Internal(gid) => chapter_to_index.get(&gid.chapter).copied(),
        crate::model::AnchorTarget::Chapter(cid) => chapter_to_index.get(cid).copied(),
        crate::model::AnchorTarget::External(_) => None,
    }
}

/// Convert source TOC entries into referenced-form KFX toc `nav_unit` entries,
/// each `target_position` retargeted to its page's image EID. An entry whose
/// target resolves to no page is dropped with its subtree.
fn build_manga_toc_entries(
    entries: &[crate::model::TocEntry],
    page_image_id: &[u64],
    chapter_to_index: &std::collections::HashMap<ChapterId, usize>,
) -> Vec<IonValue> {
    entries
        .iter()
        .filter_map(|e| {
            let idx = toc_target_index(e, chapter_to_index)?;
            let image_id = *page_image_id.get(idx)?;
            if image_id == 0 {
                return None;
            }
            let mut fields = vec![
                (
                    KfxSymbol::Representation as u64,
                    IonValue::Struct(vec![(
                        KfxSymbol::Label as u64,
                        IonValue::String(e.title.clone()),
                    )]),
                ),
                (
                    KfxSymbol::TargetPosition as u64,
                    IonValue::Struct(vec![
                        (KfxSymbol::Id as u64, IonValue::Int(image_id as i64)),
                        (KfxSymbol::Offset as u64, IonValue::Int(0)),
                    ]),
                ),
            ];
            let children = build_manga_toc_entries(&e.children, page_image_id, chapter_to_index);
            if !children.is_empty() {
                fields.push((KfxSymbol::Entries as u64, IonValue::List(children)));
            }
            Some(IonValue::Annotated(
                vec![KfxSymbol::NavUnit as u64],
                Box::new(IonValue::Struct(fields)),
            ))
        })
        .collect()
}

/// The content EIDs of a manga unit, in reading-position order: the
/// page_template opens it, then each page contributes its EIDs (cover: the bare
/// image; content: outer→inner→image).
fn manga_unit_eids(unit: &MangaUnit) -> Vec<u64> {
    let mut ids = vec![unit.pt_id];
    for p in &unit.pages {
        if unit.solo {
            ids.push(p.image_id);
        } else {
            ids.push(p.outer_id);
            ids.push(p.inner_id);
            ids.push(p.image_id);
        }
    }
    ids
}

/// position_map ($264): one entry per section listing the EIDs it contains.
fn build_manga_position_map_fragment(units: &[MangaUnit]) -> KfxFragment {
    let entries: Vec<IonValue> = units
        .iter()
        .map(|unit| {
            let ids = manga_unit_eids(unit)
                .into_iter()
                .map(|id| IonValue::Int(id as i64))
                .collect();
            IonValue::Struct(vec![
                (KfxSymbol::Contains as u64, IonValue::List(ids)),
                (
                    KfxSymbol::SectionName as u64,
                    IonValue::Symbol(unit.section_sym),
                ),
            ])
        })
        .collect();
    KfxFragment::singleton(KfxSymbol::PositionMap, IonValue::List(entries))
}

/// position_id_map ($265): section-keyed `{section_name, pid, length}` — the
/// fixed-layout form the device resolves nav targets through. `length` is the
/// section's EID count (each EID is one reading position).
fn build_manga_position_id_map_fragment(units: &[MangaUnit]) -> KfxFragment {
    let mut entries = Vec::with_capacity(units.len());
    let mut pid = 0i64;
    for unit in units {
        let length = manga_unit_eids(unit).len() as i64;
        entries.push(IonValue::Struct(vec![
            (
                KfxSymbol::SectionName as u64,
                IonValue::Symbol(unit.section_sym),
            ),
            (KfxSymbol::Pid as u64, IonValue::Int(pid)),
            (KfxSymbol::Length as u64, IonValue::Int(length)),
        ]));
        pid += length;
    }
    let ion = IonValue::Struct(vec![(KfxSymbol::Contains as u64, IonValue::List(entries))]);
    KfxFragment::singleton(KfxSymbol::PositionIdMap, ion)
}

/// section_position_id_map ($609): one entity per section walking its EIDs (span
/// 1 each), keyed by the section-name symbol. A bare int continues from the
/// previous EID + 1, else `[advance, eid]`; `[1, 0]` terminates at `length`.
fn build_manga_section_position_id_map_fragments(units: &[MangaUnit]) -> Vec<KfxFragment> {
    units
        .iter()
        .map(|unit| {
            let order = manga_unit_eids(unit);
            let mut contains: Vec<IonValue> = Vec::with_capacity(order.len() + 1);
            let mut prev: Option<u64> = None;
            for &eid in &order {
                let advance = prev.map_or(0, |_| 1);
                let consecutive = prev.is_some_and(|p| eid == p + 1);
                if consecutive {
                    contains.push(IonValue::Int(advance));
                } else {
                    contains.push(IonValue::List(vec![
                        IonValue::Int(advance),
                        IonValue::Int(eid as i64),
                    ]));
                }
                prev = Some(eid);
            }
            contains.push(IonValue::List(vec![IonValue::Int(1), IonValue::Int(0)]));
            let ion = IonValue::Struct(vec![
                (
                    KfxSymbol::SectionName as u64,
                    IonValue::Symbol(unit.section_sym),
                ),
                (KfxSymbol::Contains as u64, IonValue::List(contains)),
            ]);
            KfxFragment::new(
                KfxSymbol::SectionPositionIdMap,
                unit.section_name.clone(),
                ion,
            )
        })
        .collect()
}

/// container_entity_map ($419): the container's entity list + per-section
/// mandatory dependencies on its page resources (thumbnail then full, the pair
/// listed twice as the reference manga does).
fn build_manga_container_entity_map_fragment(
    container_id: &str,
    fragments: &[KfxFragment],
    units: &[MangaUnit],
    ctx: &ExportContext,
) -> KfxFragment {
    let mut entity_names: Vec<IonValue> = Vec::new();
    for frag in fragments {
        if frag.fid.starts_with('$') {
            continue;
        }
        if let Some(sym) = ctx.symbols.get(&frag.fid) {
            entity_names.push(IonValue::Symbol(sym));
        }
    }
    let container_entry = IonValue::Struct(vec![
        (
            KfxSymbol::Id as u64,
            IonValue::String(container_id.to_string()),
        ),
        (KfxSymbol::Contains as u64, IonValue::List(entity_names)),
    ]);

    let dependencies: Vec<IonValue> = units
        .iter()
        .map(|unit| {
            let mut pair: Vec<IonValue> = Vec::new();
            for p in &unit.pages {
                if p.thumb_sym != 0 {
                    pair.push(IonValue::Symbol(p.thumb_sym));
                }
                pair.push(IonValue::Symbol(p.res_sym));
            }
            let mut deps = pair.clone();
            deps.extend(pair);
            IonValue::Struct(vec![
                (KfxSymbol::Id as u64, IonValue::Symbol(unit.section_sym)),
                (
                    KfxSymbol::MandatoryDependencies as u64,
                    IonValue::List(deps),
                ),
            ])
        })
        .collect();

    let ion = IonValue::Struct(vec![
        (
            KfxSymbol::ContainerList as u64,
            IonValue::List(vec![container_entry]),
        ),
        (
            KfxSymbol::EntityDependencies as u64,
            IonValue::List(dependencies),
        ),
    ]);
    KfxFragment::container_entity_map(ion)
}

// PDF → KFX (fixed-layout, PDF-backed PDOC). The PDF is embedded verbatim as one
// `bcRawMedia` and the device renders each page; only the skeleton is authored —
// section/storyline/external_resource per page, PDOC metadata, feature flags.

/// Metadata stamped into a PDF→KFX (PDOC) conversion.
#[cfg(feature = "pdf")]
pub struct PdfKfxMeta {
    pub title: String,
    pub author: Option<String>,
    pub language: String,
    /// Publication date — `YYYY`, `YYYY-MM`, `YYYY-MM-DD` or a full ISO
    /// timestamp, emitted as the `issue_date` title-metadata entry truncated to
    /// the date part. `None` omits `issue_date`.
    pub date: Option<String>,
    /// Publisher imprint. Emitted as the `publisher` entry; `None`/blank yields
    /// the empty value Amazon's PDOC also carries.
    pub publisher: Option<String>,
    /// Page progression `Some("rtl")` / `Some("ltr")` / `None`, applied to
    /// `page_progression_direction` ($425) and `document_data.direction` ($192).
    pub page_progression_direction: Option<String>,
}

/// Per-page bookkeeping gathered in the survey pass.
#[cfg(feature = "pdf")]
struct PdfPageRec {
    section_name: String,
    section_sym: u64,
    story_sym: u64,
    res_sym: u64,
    /// `id` of the section's portrait page_template container.
    pt_id: u64,
    /// `id` of the section's landscape page_template container (the Amazon
    /// portrait+landscape pair; shares the same storyline as portrait).
    pt_landscape_id: u64,
    /// `id` of the storyline's outer (page-sized) container.
    container_id: u64,
    /// `id` of the image node — the renderable content EID for this page.
    image_id: u64,
    /// Symbol naming this page's `<section>-ad` auxiliary_data entity, which
    /// carries its `page_rotation`. One per page, text layer or not.
    rotation_aux_sym: u64,
    /// Text layer for this page: the "text" storyline's name symbol, the EID of
    /// the `{story_name, ignore}` child pulling it into the page container, and
    /// one record per extracted run. A page with no runs gets one too.
    text_story_sym: u64,
    text_ref_id: u64,
    /// EID of the empty page-sized container a run-less page's text storyline
    /// holds. Only meaningful when `runs` is empty; the runs carry their own.
    empty_text_id: u64,
    runs: Vec<PdfRunRec>,
}

/// Per-text-run bookkeeping: the text item's EID and the run's UTF-16 length
/// (its span in the section position map — each character is one reading
/// position).
#[cfg(feature = "pdf")]
struct PdfRunRec {
    id: u64,
    len: usize,
}

/// Symbolic name of the single shared PDF raw-media resource.
#[cfg(feature = "pdf")]
const PDF_RSRC_NAME: &str = "rsrc0";

/// Symbolic name of the optional page-1 cover JPEG resource.
#[cfg(feature = "pdf")]
const COVER_RSRC_NAME: &str = "ecover";

/// Convert a probed PDF into a fixed-layout, PDF-backed KFX (PDOC).
/// `cover_jpeg` is embedded as a loose JPEG named by `book_metadata.cover_image`,
/// part of no section. `text` adds the per-page invisible selectable overlay.
#[cfg(feature = "pdf")]
pub fn pdf_to_kfx(
    pdf: &crate::import::pdf::PdfDoc,
    meta: &PdfKfxMeta,
    cover_jpeg: Option<&[u8]>,
    text: Option<&[crate::formats::pdf::render::PageText]>,
) -> Vec<u8> {
    let container_id = generate_container_id(&meta.title);
    let mut ctx = ExportContext::new();
    let n = pdf.pages.len();

    // Page-turn direction for the PDOC, one $rtl/$ltr symbol for both sites
    let ppd_sym = ppd_symbol(meta.page_progression_direction.as_deref());

    // The whole PDF lives in one bcRawMedia entity addressed by this location;
    // every page's external_resource points here, differing only by page_index.
    let raw_location = format!("resource/{PDF_RSRC_NAME}");
    let cover_location = format!("resource/{COVER_RSRC_NAME}");

    // The runs extracted for page `i` (empty when no text layer / scanned page).
    let page_runs = |i: usize| -> &[crate::formats::pdf::render::TextRun] {
        text.and_then(|t| t.get(i))
            .map_or(&[][..], |pt| pt.runs.as_slice())
    };

    // ---- Survey: register section/story/resource symbols, allocate IDs ----
    let mut recs: Vec<PdfPageRec> = Vec::with_capacity(n);
    for i in 0..n {
        let section_name = format!("c{i}");
        let section_sym = ctx.register_section(&section_name);
        let story_sym = ctx.symbols.get_or_intern(&format!("story_c{i}"));
        let res_name = format!("e{i}");
        let res_sym = ctx.symbols.get_or_intern(&res_name);
        ctx.record_section_image_ref(&section_name, &res_name);

        let pt_id = ctx.next_fragment_id();
        let pt_landscape_id = ctx.next_fragment_id();
        let container_id_num = ctx.next_fragment_id();
        let image_id = ctx.next_fragment_id();
        ctx.record_content_length(image_id, 1);

        // The page's rotation aux and its text overlay: the storyline's name,
        // the `{story_name, ignore}` child EID, and one EID per run carrying the
        // run's UTF-16 length. A textless page gets the pair too.
        let rotation_aux_sym = ctx.symbols.get_or_intern(&format!("{section_name}-ad"));
        let runs = page_runs(i);
        let text_story_sym = ctx.symbols.get_or_intern(&format!("tstory_c{i}"));
        let text_ref_id = ctx.next_fragment_id();
        let run_recs: Vec<PdfRunRec> = runs
            .iter()
            .map(|run| PdfRunRec {
                id: ctx.next_fragment_id(),
                len: run.content.encode_utf16().count(),
            })
            .collect();
        let empty_text_id = if run_recs.is_empty() {
            ctx.next_fragment_id()
        } else {
            0
        };

        recs.push(PdfPageRec {
            section_name,
            section_sym,
            story_sym,
            res_sym,
            pt_id,
            pt_landscape_id,
            container_id: container_id_num,
            image_id,
            rotation_aux_sym,
            text_story_sym,
            text_ref_id,
            empty_text_id,
            runs: run_recs,
        });
    }
    // Intern the shared raw-media location; the bcRawMedia entity resolves on it.
    ctx.symbols.get_or_intern(&raw_location);

    // Optional page-1 cover: a loose JPEG resource (external_resource +
    // bcRawMedia) referenced by `book_metadata.cover_image`. Symbols surveyed
    // here as `(res_sym, width_px, height_px)`.
    let cover: Option<(u64, u32, u32)> = cover_jpeg.map(|jpeg| {
        let res_sym = ctx.symbols.get_or_intern(COVER_RSRC_NAME);
        ctx.symbols.get_or_intern(&cover_location);
        let (w, h) = crate::util::extract_image_dimensions(jpeg).unwrap_or((0, 0));
        (res_sym, w, h)
    });

    // Extracted runs make the book's text live, gating the capability flags
    let has_text = recs.iter().any(|r| !r.runs.is_empty());

    // Resource-descriptor aux (Amazon's `d6`/`d7`): `d6` describes the embedded
    // PDF resource, `d7` lists `[d6]`, and `document_data` points at `d7`.

    // ---- Synthesis: build fragments in reference entity order ----
    let mut fragments: Vec<KfxFragment> = Vec::new();

    // 1. content_features ($585)
    fragments.push(build_pdf_content_features_fragment(
        has_text,
        pdf.pages.iter().any(|p| p.rotation != 0),
    ));
    // 2. book_metadata ($490) — PDOC (with cover_image when a cover is present)
    fragments.push(build_pdf_book_metadata_fragment(
        meta,
        &container_id,
        pdf,
        cover.map(|_| COVER_RSRC_NAME),
        has_text,
    ));
    // 3. metadata ($258) — reading order
    fragments.push(build_fxl_metadata_fragment(&ctx, ppd_sym, None));
    // 4. document_data ($538) — inserted here later, once max_id is known.
    let document_data_index = fragments.len();

    // Sections, storylines, external_resources (grouped like the EPUB path). A
    // page with extracted runs gets a second, invisible "text" storyline holding
    // the selectable overlay; its page-image storyline references that by name.
    let mut sections = Vec::with_capacity(n);
    let mut storylines = Vec::with_capacity(n);
    let mut text_storylines = Vec::new();
    let mut resources = Vec::with_capacity(n);
    for (i, rec) in recs.iter().enumerate() {
        let page = pdf.pages[i];
        sections.push(build_pdf_page_section(rec));
        storylines.push(build_pdf_page_storyline(rec, page.width, page.height));
        text_storylines.push(build_pdf_text_storyline(
            rec,
            page_runs(i),
            page.width,
            page.height,
        ));
        resources.push(build_pdf_external_resource(
            rec,
            i,
            page.width,
            page.height,
            page_runs(i),
            &raw_location,
        ));
    }
    fragments.extend(sections);
    fragments.extend(storylines);
    fragments.extend(text_storylines);

    // auxiliary_data ($597): one `<section>-ad` per page stating the page's
    // rotation, the entire aux set of a Send-to-Kindle PDF KFX. Standalone, not
    // referenced from the section; the reader finds it by name.
    for (i, rec) in recs.iter().enumerate() {
        fragments.push(build_kv_aux_fragment(
            &format!("{}-ad", rec.section_name),
            rec.rotation_aux_sym,
            "page_rotation",
            IonValue::Int(pdf.pages[i].rotation as i64),
        ));
    }

    // external_resource entities (pages, then the cover), then the bcRawMedia
    // blobs (shared PDF, then the cover JPEG).
    fragments.extend(resources);
    if let (Some((res_sym, w, h)), Some(jpeg)) = (cover, cover_jpeg) {
        fragments.push(build_pdf_cover_external_resource(
            res_sym,
            w,
            h,
            &cover_location,
            jpeg,
        ));
    }
    fragments.push(KfxFragment::raw(
        KfxSymbol::Bcrawmedia as u64,
        &raw_location,
        pdf.bytes.clone(),
    ));
    if let Some(jpeg) = cover_jpeg {
        fragments.push(KfxFragment::raw(
            KfxSymbol::Bcrawmedia as u64,
            &cover_location,
            jpeg.to_vec(),
        ));
    }

    // Navigation: `nav_container` ($391) entities + a thin `book_navigation`
    fragments.extend(build_pdf_nav_fragments(pdf, &recs, &mut ctx));

    // Position system: `position_id_map` (section → pid range) plus
    // `section_position_id_map` (per-section position → EID). Both are required
    // for the overlay text to be selectable.
    fragments.push(build_pdf_position_map_fragment(&recs));
    fragments.push(build_pdf_position_id_map_fragment(&recs));
    fragments.extend(build_pdf_section_position_id_map_fragments(&recs));
    // No `location_map` ($550): Amazon's PDF KFX has none, and page `image_id`s
    // are absent from the section-keyed `position_id_map`.

    // Container metadata.
    fragments.push(build_resource_path_fragment());
    fragments.push(build_pdf_container_entity_map_fragment(
        &container_id,
        &fragments,
        &recs,
        &raw_location,
        cover.map(|(res_sym, _, _)| (res_sym, cover_location.as_str())),
        &ctx,
    ));

    // document_data, with every fragment ID allocated (max_id correct).
    fragments.insert(
        document_data_index,
        build_pdf_document_data_fragment(&ctx, ppd_sym),
    );

    // ---- Serialize ----
    let symtab_ion = build_symbol_table_ion(ctx.symbols.local_symbols());
    let format_caps_ion = build_format_capabilities_ion();
    let entities = serialize_fragments(&fragments, ctx.symbols.local_symbols());
    serialize_container(&container_id, &entities, &symtab_ion, &format_caps_ion)
}

/// A percent dimension struct: `{ value: 100, unit: percent }`.
fn percent_100() -> IonValue {
    IonValue::Struct(vec![
        (KfxSymbol::Value as u64, IonValue::Int(100)),
        (
            KfxSymbol::Unit as u64,
            IonValue::Symbol(KfxSymbol::Percent as u64),
        ),
    ])
}

/// Build the storyline ($259) for one PDF page: a page-sized container holding
/// the PDF page as a 100%×100% image (Amazon's `l2`), plus — for a page with
/// runs — a `links_extracted` marker and the invisible text-overlay child.
#[cfg(feature = "pdf")]
fn build_pdf_page_storyline(rec: &PdfPageRec, width_pt: f32, height_pt: f32) -> KfxFragment {
    // Amazon sizes the page container in points×100.
    let fixed_w = (width_pt * 100.0).round() as i64;
    let fixed_h = (height_pt * 100.0).round() as i64;

    let image = IonValue::Struct(vec![
        (KfxSymbol::Id as u64, IonValue::Int(rec.image_id as i64)),
        (KfxSymbol::Width as u64, percent_100()),
        (KfxSymbol::Height as u64, percent_100()),
        (
            KfxSymbol::Type as u64,
            IonValue::Symbol(KfxSymbol::Image as u64),
        ),
        (
            KfxSymbol::ResourceName as u64,
            IonValue::Symbol(rec.res_sym),
        ),
    ]);

    // Container content: the PDF page image, then the text-storyline reference
    // marked `ignore: true` (the invisible overlay). Present on every page —
    // see `PdfPageRec::text_story_sym`.
    let content = vec![
        image,
        IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Int(rec.text_ref_id as i64)),
            (
                KfxSymbol::StoryName as u64,
                IonValue::Symbol(rec.text_story_sym),
            ),
            (
                KfxSymbol::Layout as u64,
                IonValue::Symbol(KfxSymbol::Fixed as u64),
            ),
            (KfxSymbol::Ignore as u64, IonValue::Bool(true)),
            (
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::Container as u64),
            ),
        ]),
    ];

    let mut container_fields = vec![
        (KfxSymbol::Id as u64, IonValue::Int(rec.container_id as i64)),
        (KfxSymbol::FixedWidth as u64, IonValue::Int(fixed_w)),
        (KfxSymbol::FixedHeight as u64, IonValue::Int(fixed_h)),
        (
            KfxSymbol::FitText as u64,
            IonValue::Symbol(KfxSymbol::Force as u64),
        ),
        (
            KfxSymbol::Layout as u64,
            IonValue::Symbol(KfxSymbol::ScaleFit as u64),
        ),
        (
            KfxSymbol::Float as u64,
            IonValue::Symbol(KfxSymbol::Center as u64),
        ),
    ];
    container_fields.push((
        KfxSymbol::Type as u64,
        IonValue::Symbol(KfxSymbol::Container as u64),
    ));
    container_fields.push((KfxSymbol::ContentList as u64, IonValue::List(content)));
    let container = IonValue::Struct(container_fields);

    let ion = IonValue::Struct(vec![
        (KfxSymbol::StoryName as u64, IonValue::Symbol(rec.story_sym)),
        (
            KfxSymbol::ContentList as u64,
            IonValue::List(vec![container]),
        ),
    ]);
    KfxFragment::new(
        KfxSymbol::Storyline,
        format!("story_{}", rec.section_name),
        ion,
    )
}

/// Build the invisible "text" storyline ($259) for one PDF page — the selectable
/// overlay (Amazon's `l1SJ`). Each run is a `type: text` at fixed `top`/`left`
/// with `visibility: false`, word-segmented and linked to its `text_baseline`.
#[cfg(feature = "pdf")]
fn build_pdf_text_storyline(
    rec: &PdfPageRec,
    runs: &[crate::formats::pdf::render::TextRun],
    width_pt: f32,
    height_pt: f32,
) -> KfxFragment {
    let items: Vec<IonValue> = rec
        .runs
        .iter()
        .zip(runs)
        .map(|(rr, run)| {
            let style_events: Vec<IonValue> = run
                .words
                .iter()
                .map(|w| {
                    let mut ev = vec![
                        (KfxSymbol::Offset as u64, IonValue::Int(w.offset as i64)),
                        (KfxSymbol::Length as u64, IonValue::Int(w.length as i64)),
                        (KfxSymbol::Width as u64, IonValue::Int(w.width)),
                        (KfxSymbol::Visibility as u64, IonValue::Bool(false)),
                    ];
                    if w.is_word {
                        ev.push((
                            KfxSymbol::Model as u64,
                            IonValue::Symbol(KfxSymbol::Word as u64),
                        ));
                    }
                    IonValue::Struct(ev)
                })
                .collect();

            IonValue::Struct(vec![
                (KfxSymbol::Id as u64, IonValue::Int(rr.id as i64)),
                (
                    KfxSymbol::Position as u64,
                    IonValue::Symbol(KfxSymbol::Fixed as u64),
                ),
                (KfxSymbol::Width as u64, IonValue::Int(run.width)),
                (KfxSymbol::Height as u64, IonValue::Int(run.height)),
                (KfxSymbol::Top as u64, IonValue::Int(run.top)),
                (KfxSymbol::Left as u64, IonValue::Int(run.left)),
                (
                    KfxSymbol::WordIterationType as u64,
                    IonValue::Symbol(KfxSymbol::Model as u64),
                ),
                (
                    KfxSymbol::Type as u64,
                    IonValue::Symbol(KfxSymbol::Text as u64),
                ),
                (KfxSymbol::StyleEvents as u64, IonValue::List(style_events)),
                (
                    KfxSymbol::Content as u64,
                    IonValue::String(run.content.clone()),
                ),
            ])
        })
        .collect();

    // A page that yielded no runs gets a storyline holding one empty page-sized
    // container — what Amazon puts there, keeping every page addressable through
    // the same overlay EID.
    let items = if items.is_empty() {
        vec![IonValue::Struct(vec![
            (
                KfxSymbol::Id as u64,
                IonValue::Int(rec.empty_text_id as i64),
            ),
            (
                KfxSymbol::Width as u64,
                IonValue::Int((width_pt * 100.0).round() as i64),
            ),
            (
                KfxSymbol::Height as u64,
                IonValue::Int((height_pt * 100.0).round() as i64),
            ),
            (KfxSymbol::Top as u64, IonValue::Int(0)),
            (KfxSymbol::Left as u64, IonValue::Int(0)),
            (
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::Container as u64),
            ),
            (
                KfxSymbol::Position as u64,
                IonValue::Symbol(KfxSymbol::Fixed as u64),
            ),
        ])]
    } else {
        items
    };

    let ion = IonValue::Struct(vec![
        (
            KfxSymbol::StoryName as u64,
            IonValue::Symbol(rec.text_story_sym),
        ),
        (KfxSymbol::ContentList as u64, IonValue::List(items)),
    ]);
    KfxFragment::new(
        KfxSymbol::Storyline,
        format!("tstory_{}", rec.section_name),
        ion,
    )
}

/// Build an `auxiliary_data` ($597) fragment: `{kfx_id: <sym>, metadata: [{key,
/// value}, …]}`. `fid` must be the string the `kfx_id` symbol was interned from.
#[cfg(feature = "pdf")]
fn build_aux_fragment(fid: &str, kfx_id_sym: u64, entries: Vec<(&str, IonValue)>) -> KfxFragment {
    let metadata: Vec<IonValue> = entries
        .into_iter()
        .map(|(key, value)| {
            IonValue::Struct(vec![
                (KfxSymbol::Key as u64, IonValue::String(key.to_string())),
                (KfxSymbol::Value as u64, value),
            ])
        })
        .collect();
    let ion = IonValue::Struct(vec![
        (KfxSymbol::KfxId as u64, IonValue::Symbol(kfx_id_sym)),
        (KfxSymbol::Metadata as u64, IonValue::List(metadata)),
    ]);
    KfxFragment::new(KfxSymbol::AuxiliaryData, fid, ion)
}

/// Build a one-entry `auxiliary_data` fragment (the text layer's
/// `links_extracted` / `text_baseline` entries).
#[cfg(feature = "pdf")]
fn build_kv_aux_fragment(fid: &str, kfx_id_sym: u64, key: &str, value: IonValue) -> KfxFragment {
    build_aux_fragment(fid, kfx_id_sym, vec![(key, value)])
}

/// Build the section ($260) for one PDF page: Amazon's portrait+landscape
/// `page_template` pair over one storyline, selected on device by the
/// `condition: (isPortrait)` / `(isLandscape)` s-expression.
#[cfg(feature = "pdf")]
fn build_pdf_page_section(rec: &PdfPageRec) -> KfxFragment {
    let portrait = IonValue::Struct(vec![
        (KfxSymbol::Id as u64, IonValue::Int(rec.pt_id as i64)),
        (KfxSymbol::StoryName as u64, IonValue::Symbol(rec.story_sym)),
        (
            KfxSymbol::Condition as u64,
            IonValue::Sexp(vec![IonValue::Symbol(KfxSymbol::Isportrait as u64)]),
        ),
        (
            KfxSymbol::Layout as u64,
            IonValue::Symbol(KfxSymbol::Vertical as u64),
        ),
        (
            KfxSymbol::Type as u64,
            IonValue::Symbol(KfxSymbol::Container as u64),
        ),
    ]);
    let landscape = IonValue::Struct(vec![
        (
            KfxSymbol::Id as u64,
            IonValue::Int(rec.pt_landscape_id as i64),
        ),
        (KfxSymbol::Width as u64, percent_100()),
        (KfxSymbol::StoryName as u64, IonValue::Symbol(rec.story_sym)),
        (KfxSymbol::FixedWidth as u64, percent_100()),
        (
            KfxSymbol::Condition as u64,
            IonValue::Sexp(vec![IonValue::Symbol(KfxSymbol::Islandscape as u64)]),
        ),
        (
            KfxSymbol::Layout as u64,
            IonValue::Symbol(KfxSymbol::Overflow as u64),
        ),
        (
            KfxSymbol::Type as u64,
            IonValue::Symbol(KfxSymbol::Container as u64),
        ),
    ]);
    let ion = IonValue::Struct(vec![
        (
            KfxSymbol::SectionName as u64,
            IonValue::Symbol(rec.section_sym),
        ),
        (
            KfxSymbol::PageTemplates as u64,
            IonValue::List(vec![portrait, landscape]),
        ),
    ]);
    KfxFragment::new(KfxSymbol::Section, &rec.section_name, ion)
}

/// A length in points, as Amazon writes it: an `Int` on a whole point, an Ion
/// decimal below. Decimal keeps the exact base-10 quantity — `hundredths` counts
/// pt×100 — where a binary float prints 30.219999999999999.
#[cfg(feature = "pdf")]
fn pt_value(hundredths: i64) -> IonValue {
    if hundredths % 100 == 0 {
        return IonValue::Int(hundredths / 100);
    }
    let sign = if hundredths < 0 { "-" } else { "" };
    let h = hundredths.abs();
    IonValue::Decimal(format!("{sign}{}.{:02}", h / 100, h % 100))
}

/// A page dimension in points, at the precision the PDF states it. Not
/// [`pt_value`]: a MediaBox of `351.496 × 598.11` quantized to pt×100 rounds the
/// width to `351.5`. The trim sets the final width.
#[cfg(feature = "pdf")]
fn pt_page_dim(pt: f32) -> IonValue {
    if pt.fract() == 0.0 && pt.abs() < i64::MAX as f32 {
        return IonValue::Int(pt as i64);
    }
    let text = format!("{pt:.4}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    IonValue::Decimal(trimmed.to_string())
}

/// The page's content box, as the insets from each page edge Amazon stores in
/// `margin_left`/`_top`/`_right`/`_bottom`. `None` for a page with no text layer,
/// which Amazon writes as a flat `margin: 0`. Everything here is pt×100.
#[cfg(feature = "pdf")]
fn pdf_page_margins(
    runs: &[crate::formats::pdf::render::TextRun],
    page: (i64, i64),
) -> Option<[i64; 4]> {
    let left = runs.iter().map(|r| r.left).min()?;
    let top = runs.iter().map(|r| r.top).min()?;
    let right = runs.iter().map(|r| r.left + r.width).max()?;
    let bottom = runs.iter().map(|r| r.top + r.height).max()?;
    // Clamped: a run may overhang the MediaBox (a bleed glyph, or a rotated
    // page whose bounds were taken unrotated), and a negative inset crops
    // outwards.
    Some([
        left.max(0),
        top.max(0),
        (page.0 - right).max(0),
        (page.1 - bottom).max(0),
    ])
}

/// Build the external_resource ($164) for one PDF page: a `format: pdf` view of
/// the shared blob at `page_index`, sized to the page and carrying the content
/// box the reader's margin setting crops to. See [`pdf_page_margins`].
#[cfg(feature = "pdf")]
fn build_pdf_external_resource(
    rec: &PdfPageRec,
    page_index: usize,
    width_pt: f32,
    height_pt: f32,
    runs: &[crate::formats::pdf::render::TextRun],
    raw_location: &str,
) -> KfxFragment {
    let page = (
        (width_pt * 100.0).round() as i64,
        (height_pt * 100.0).round() as i64,
    );
    let mut fields = vec![
        (
            KfxSymbol::Format as u64,
            IonValue::Symbol(KfxSymbol::Pdf as u64),
        ),
        (
            KfxSymbol::PageIndex as u64,
            IonValue::Int(page_index as i64),
        ),
        (
            KfxSymbol::Location as u64,
            IonValue::String(raw_location.to_string()),
        ),
        (KfxSymbol::ResourceWidth as u64, pt_page_dim(width_pt)),
        (KfxSymbol::ResourceHeight as u64, pt_page_dim(height_pt)),
        (
            KfxSymbol::ResourceName as u64,
            IonValue::Symbol(rec.res_sym),
        ),
    ];
    match pdf_page_margins(runs, page) {
        Some([left, top, right, bottom]) => fields.extend([
            (KfxSymbol::MarginLeft as u64, pt_value(left)),
            (KfxSymbol::MarginTop as u64, pt_value(top)),
            (KfxSymbol::MarginRight as u64, pt_value(right)),
            (KfxSymbol::MarginBottom as u64, pt_value(bottom)),
        ]),
        // No text layer states where this page's ink stops — the flat
        // `margin: 0` Amazon writes for its own contentless page.
        None => fields.push((KfxSymbol::Margin as u64, IonValue::Int(0))),
    }
    KfxFragment::new(
        KfxSymbol::ExternalResource,
        format!("e{page_index}"),
        IonValue::Struct(fields),
    )
}

/// Build the external_resource ($164) for the page-1 cover JPEG: a loose image
/// resource (a real JPEG, no `page_index`) referenced by
/// `book_metadata.cover_image`, part of no section.
#[cfg(feature = "pdf")]
fn build_pdf_cover_external_resource(
    res_sym: u64,
    width_px: u32,
    height_px: u32,
    location: &str,
    jpeg: &[u8],
) -> KfxFragment {
    let mut fields = vec![
        (KfxSymbol::ResourceName as u64, IonValue::Symbol(res_sym)),
        (
            KfxSymbol::Location as u64,
            IonValue::String(location.to_string()),
        ),
        (
            KfxSymbol::Format as u64,
            IonValue::Symbol(detect_format_symbol("cover.jpg", jpeg)),
        ),
    ];
    if width_px > 0 && height_px > 0 {
        fields.push((
            KfxSymbol::ResourceWidth as u64,
            IonValue::Int(width_px as i64),
        ));
        fields.push((
            KfxSymbol::ResourceHeight as u64,
            IonValue::Int(height_px as i64),
        ));
    }
    if let Some(mime) = crate::util::detect_mime_type("cover.jpg", jpeg) {
        fields.push((KfxSymbol::Mime as u64, IonValue::String(mime.to_string())));
    }
    KfxFragment::new(
        KfxSymbol::ExternalResource,
        COVER_RSRC_NAME,
        IonValue::Struct(fields),
    )
}

/// content_features ($585) for a PDF-backed fixed-layout book. `yj_pdf_links`
/// stays off until link-annotation extraction exists.
#[cfg(feature = "pdf")]
fn build_pdf_content_features_fragment(has_text: bool, has_rotated_pages: bool) -> KfxFragment {
    fn feature(namespace: &str, key: &str, major: i64) -> IonValue {
        IonValue::Struct(vec![
            (
                KfxSymbol::Namespace as u64,
                IonValue::String(namespace.to_string()),
            ),
            (KfxSymbol::Key as u64, IonValue::String(key.to_string())),
            (
                KfxSymbol::VersionInfo as u64,
                IonValue::Struct(vec![(
                    KfxSymbol::Version as u64,
                    IonValue::Struct(vec![
                        (KfxSymbol::MajorVersion as u64, IonValue::Int(major)),
                        (KfxSymbol::MinorVersion as u64, IonValue::Int(0)),
                    ]),
                )]),
            ),
        ])
    }
    const YJ: &str = "com.amazon.yjconversion";
    let mut feats = vec![
        feature("SDK.Marker", "CanonicalFormat", 2),
        feature(YJ, "yj_fixed_layout", 1),
        feature(YJ, "yj_graphical_highlights", 1),
        feature(YJ, "yj_textbook", 1),
        feature(YJ, "yj_pdf_support", 1),
    ];
    // The custom word iterator backs the selectable text layer's word model;
    // only advertise it when there actually is a text layer.
    if has_text {
        feats.push(feature(YJ, "yj_custom_word_iterator", 1));
    }
    if has_rotated_pages {
        feats.push(feature(YJ, "yj_rotated_pages", 1));
    }
    let ion = IonValue::Struct(vec![(KfxSymbol::Features as u64, IonValue::List(feats))]);
    KfxFragment::singleton(KfxSymbol::ContentFeatures, ion)
}

/// book_metadata ($490) for a PDOC, mirroring "Send to Kindle" categories.
#[cfg(feature = "pdf")]
fn build_pdf_book_metadata_fragment(
    meta: &PdfKfxMeta,
    container_id: &str,
    pdf: &crate::import::pdf::PdfDoc,
    cover_resource_name: Option<&str>,
    has_text: bool,
) -> KfxFragment {
    fn kv(key: &str, value: IonValue) -> IonValue {
        IonValue::Struct(vec![
            (KfxSymbol::Key as u64, IonValue::String(key.to_string())),
            (KfxSymbol::Value as u64, value),
        ])
    }
    fn category(name: &str, entries: Vec<IonValue>) -> IonValue {
        IonValue::Struct(vec![
            (
                KfxSymbol::Category as u64,
                IonValue::String(name.to_string()),
            ),
            (KfxSymbol::Metadata as u64, IonValue::List(entries)),
        ])
    }

    let content_id = synth_pdoc_content_id(meta, pdf);
    let book_id = generate_book_id(&content_id);

    let mut title_entries = vec![kv("book_id", IonValue::String(book_id))];
    // `issue_date` sits right after `book_id` (Amazon's PDOC field order). Emit
    // it only when the library carries a date, truncated to the YYYY-MM-DD the
    // KFX expects (a bare year passes through unchanged).
    if let Some(date) = meta
        .date
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        title_entries.push(kv(
            "issue_date",
            IonValue::String(crate::util::truncate_to_date(date)),
        ));
    }
    title_entries.push(kv("content_id", IonValue::String(content_id.clone())));
    if let Some(author) = &meta.author {
        title_entries.push(kv("author", IonValue::String(author.clone())));
    }
    let publisher = meta.publisher.as_deref().map(str::trim).unwrap_or("");
    title_entries.extend([
        kv("cde_content_type", IonValue::String("PDOC".to_string())),
        kv("ASIN", IonValue::String(content_id.clone())),
        kv("publisher", IonValue::String(publisher.to_string())),
        kv("language", IonValue::String(meta.language.clone())),
        kv("title", IonValue::String(meta.title.clone())),
    ]);
    // The cover_image value is the cover resource's symbolic name, matched
    // against an external_resource, mirroring the EPUB path. PDOC + a
    // synthesized ASIN renders the Kindle tile from this embedded image.
    if let Some(name) = cover_resource_name {
        title_entries.push(kv("cover_image", IonValue::String(name.to_string())));
    }
    title_entries.extend([
        kv("is_sample", IonValue::Bool(false)),
        kv("asset_id", IonValue::String(container_id.to_string())),
    ]);

    let categorised = IonValue::List(vec![
        category(
            "kindle_audit_metadata",
            vec![
                kv("file_creator", IonValue::String("bokai".to_string())),
                kv(
                    "creator_version",
                    IonValue::String(env!("CARGO_PKG_VERSION").to_string()),
                ),
            ],
        ),
        category("kindle_title_metadata", title_entries),
        category(
            "kindle_capability_metadata",
            vec![
                kv("yj_fixed_layout", IonValue::Int(1)),
                kv("yj_textbook", IonValue::Int(1)),
                kv("graphical_highlights", IonValue::Int(1)),
            ],
        ),
        category("kindle_ebook_metadata", {
            let mut e = vec![
                kv(
                    "book_orientation_lock",
                    IonValue::String("none".to_string()),
                ),
                kv(
                    "user_visible_labeling",
                    IonValue::String("page_exclusive".to_string()),
                ),
            ];
            // The selectable text layer (Amazon's `selection: enabled`,
            // `multipage_selection: disabled`) — only when there's live text.
            if has_text {
                e.push(kv("selection", IonValue::String("enabled".to_string())));
                e.push(kv(
                    "multipage_selection",
                    IonValue::String("disabled".to_string()),
                ));
            }
            e
        }),
    ]);

    let ion = IonValue::Struct(vec![(KfxSymbol::CategorisedMetadata as u64, categorised)]);
    KfxFragment::singleton(KfxSymbol::BookMetadata, ion)
}

/// Deterministic content_id/ASIN for a PDOC via
/// [`crate::formats::kfx::metadata::generate_content_id`], seeded by the PDF's
/// stable identity: title + author + byte size + page count.
#[cfg(feature = "pdf")]
fn synth_pdoc_content_id(meta: &PdfKfxMeta, pdf: &crate::import::pdf::PdfDoc) -> String {
    let seed = format!(
        "{}\u{0}{}\u{0}{}\u{0}{}",
        meta.title,
        meta.author.as_deref().unwrap_or(""),
        pdf.bytes.len(),
        pdf.pages.len(),
    );
    crate::formats::kfx::metadata::generate_content_id(&seed)
}

/// Resolve a page-progression-direction string to its KFX symbol: `"rtl"` →
/// `$rtl` (375), `"ltr"` → `$ltr` (376); anything else (incl. `None`) → `None`,
/// meaning "omit the field" — the device then defaults to ltr.
#[cfg(feature = "pdf")]
fn ppd_symbol(ppd: Option<&str>) -> Option<KfxSymbol> {
    match ppd {
        Some("rtl") => Some(KfxSymbol::Rtl),
        Some("ltr") => Some(KfxSymbol::Ltr),
        _ => None,
    }
}

/// Build a default reading order over all sections, appending the
/// `page_progression_direction` ($425) symbol when `ppd_sym` is set.
fn default_reading_order(ctx: &ExportContext, ppd_sym: Option<KfxSymbol>) -> IonValue {
    let sections: Vec<IonValue> = ctx
        .section_ids
        .iter()
        .map(|&id| IonValue::Symbol(id))
        .collect();
    let mut fields = vec![
        (
            KfxSymbol::ReadingOrderName as u64,
            IonValue::Symbol(KfxSymbol::Default as u64),
        ),
        (KfxSymbol::Sections as u64, IonValue::List(sections)),
    ];
    if let Some(sym) = ppd_sym {
        fields.push((
            KfxSymbol::PageProgressionDirection as u64,
            IonValue::Symbol(sym as u64),
        ));
    }
    IonValue::Struct(fields)
}

/// metadata ($258): the default reading order over all sections.
fn build_fxl_metadata_fragment(
    ctx: &ExportContext,
    ppd_sym: Option<KfxSymbol>,
    cover_resource: Option<u64>,
) -> KfxFragment {
    // A fixed-layout book's cover rides here, ahead of the reading order — the
    // slot Amazon's own manga uses, and the one its comic reader reads.
    let mut fields = Vec::new();
    if let Some(sym) = cover_resource {
        fields.push((KfxSymbol::CoverImage as u64, IonValue::Symbol(sym)));
    }
    fields.push((
        KfxSymbol::ReadingOrders as u64,
        IonValue::List(vec![default_reading_order(ctx, ppd_sym)]),
    ));
    KfxFragment::singleton(KfxSymbol::Metadata, IonValue::Struct(fields))
}

/// document_data ($538): minimal fixed-layout document — max_id, pan_zoom and
/// the reading order, which is all Amazon's PDF document_data carries. (Reflow
/// fields like font_size/line_height are irrelevant to a PDF-backed book.)
#[cfg(feature = "pdf")]
fn build_pdf_document_data_fragment(
    ctx: &ExportContext,
    ppd_sym: Option<KfxSymbol>,
) -> KfxFragment {
    let reading_order = default_reading_order(ctx, ppd_sym);
    let mut fields = vec![
        (KfxSymbol::MaxId as u64, IonValue::Int(ctx.max_eid() as i64)),
        (
            KfxSymbol::PanZoom as u64,
            IonValue::Symbol(KfxSymbol::Enabled as u64),
        ),
        (
            KfxSymbol::ReadingOrders as u64,
            IonValue::List(vec![reading_order]),
        ),
    ];
    // The page progression also reaches `document_data.direction` ($192): a
    // fixed-layout PDOC has no writing_mode, and this field is the
    // document-level signal pairing with the reading order's $425.
    if let Some(sym) = ppd_sym {
        fields.push((KfxSymbol::Direction as u64, IonValue::Symbol(sym as u64)));
    }
    KfxFragment::singleton(KfxSymbol::DocumentData, IonValue::Struct(fields))
}

/// position_map ($264): one entry per page enumerating the section's content
/// EIDs — for a text page the text-ref child and every text item too, matching
/// Amazon's `[content_start, count]` run over image..last-text-item.
#[cfg(feature = "pdf")]
fn pdf_section_span(rec: &PdfPageRec) -> i64 {
    let mut span = 5i64; // pt_id + container + image + pt_landscape + text_ref
    if rec.runs.is_empty() {
        span += 1; // the empty page-sized container that stands in for the runs
    } else {
        span += rec.runs.iter().map(|r| r.len as i64).sum::<i64>();
    }
    span
}

/// position_map ($264): one entry per page enumerating the section's EIDs — both
/// page templates, container, image, and (for a text page) the text-ref child +
/// every text item. Tells the device which EIDs belong to each section.
#[cfg(feature = "pdf")]
fn build_pdf_position_map_fragment(recs: &[PdfPageRec]) -> KfxFragment {
    let entries: Vec<IonValue> = recs
        .iter()
        .map(|rec| {
            let mut ids = vec![
                IonValue::Int(rec.pt_id as i64),
                IonValue::Int(rec.pt_landscape_id as i64),
                IonValue::Int(rec.container_id as i64),
                IonValue::Int(rec.image_id as i64),
            ];
            ids.push(IonValue::Int(rec.text_ref_id as i64));
            if rec.runs.is_empty() {
                ids.push(IonValue::Int(rec.empty_text_id as i64));
            } else {
                ids.extend(rec.runs.iter().map(|r| IonValue::Int(r.id as i64)));
            }
            IonValue::Struct(vec![
                (KfxSymbol::Contains as u64, IonValue::List(ids)),
                (
                    KfxSymbol::SectionName as u64,
                    IonValue::Symbol(rec.section_sym),
                ),
            ])
        })
        .collect();
    KfxFragment::singleton(KfxSymbol::PositionMap, IonValue::List(entries))
}

/// position_id_map ($265) for a fixed-layout (PDF) book: `{contains:
/// [{section_name, pid, length}, …]}` — `pid` the section's cumulative start,
/// `length` its span. Paired with `section_position_id_map`.
#[cfg(feature = "pdf")]
fn build_pdf_position_id_map_fragment(recs: &[PdfPageRec]) -> KfxFragment {
    let mut entries: Vec<IonValue> = Vec::with_capacity(recs.len());
    let mut pid = 0i64;
    for rec in recs {
        let length = pdf_section_span(rec);
        entries.push(IonValue::Struct(vec![
            (
                KfxSymbol::SectionName as u64,
                IonValue::Symbol(rec.section_sym),
            ),
            (KfxSymbol::Pid as u64, IonValue::Int(pid)),
            (KfxSymbol::Length as u64, IonValue::Int(length)),
        ]));
        pid += length;
    }
    let ion = IonValue::Struct(vec![(KfxSymbol::Contains as u64, IonValue::List(entries))]);
    KfxFragment::singleton(KfxSymbol::PositionIdMap, ion)
}

/// section_position_id_map ($609): one entity per section mapping reading
/// positions to EIDs. Each element advances the pid by the PREVIOUS EID's span
/// and names the current EID; `[advance, 0]` terminates at `pid == length`.
#[cfg(feature = "pdf")]
fn build_pdf_section_position_id_map_fragments(recs: &[PdfPageRec]) -> Vec<KfxFragment> {
    recs.iter()
        .map(|rec| {
            // (eid, span) in reading-position order: the portrait page_template
            // opens the section and the landscape one closes it. Amazon's section
            // anchors are these page_template EIDs, real backed elements.
            let mut order: Vec<(u64, i64)> =
                vec![(rec.pt_id, 1), (rec.container_id, 1), (rec.image_id, 1)];
            order.push((rec.text_ref_id, 1));
            if rec.runs.is_empty() {
                order.push((rec.empty_text_id, 1));
            } else {
                order.extend(rec.runs.iter().map(|r| (r.id, r.len as i64)));
            }
            order.push((rec.pt_landscape_id, 1));

            let mut contains: Vec<IonValue> = Vec::with_capacity(order.len() + 1);
            let mut prev: Option<(u64, i64)> = None;
            for &(eid, span) in &order {
                let advance = prev.map_or(0, |(_, s)| s);
                let consecutive = prev.is_some_and(|(p, _)| eid == p + 1);
                if consecutive {
                    contains.push(IonValue::Int(advance));
                } else {
                    contains.push(IonValue::List(vec![
                        IonValue::Int(advance),
                        IonValue::Int(eid as i64),
                    ]));
                }
                prev = Some((eid, span));
            }
            // Terminator: advance by the last EID's span; eid 0.
            let last_span = prev.map_or(0, |(_, s)| s);
            contains.push(IonValue::List(vec![
                IonValue::Int(last_span),
                IonValue::Int(0),
            ]));

            let ion = IonValue::Struct(vec![
                (
                    KfxSymbol::SectionName as u64,
                    IonValue::Symbol(rec.section_sym),
                ),
                (KfxSymbol::Contains as u64, IonValue::List(contains)),
            ]);
            // Key the entity by the SECTION NAME symbol, as Amazon does: a
            // `section` and its `section_position_id_map` share it. An uninterned
            // name serializes to id $0 and the device rejects the book.
            KfxFragment::new(
                KfxSymbol::SectionPositionIdMap,
                rec.section_name.clone(),
                ion,
            )
        })
        .collect()
}

/// Navigation fragments for the PDF TOC in Amazon's shape: a separate
/// `nav_container` ($391) entity holding the table of contents and a thin
/// `book_navigation` ($389) referencing it by name. Empty without an outline.
#[cfg(feature = "pdf")]
fn build_pdf_nav_fragments(
    pdf: &crate::import::pdf::PdfDoc,
    recs: &[PdfPageRec],
    ctx: &mut ExportContext,
) -> Vec<KfxFragment> {
    // A named nav_container entity holding `entries` of the given `nav_type`.
    // book_navigation references it by the returned symbol.
    let mut make_container = |name: &str, nav_type: KfxSymbol, entries: Vec<IonValue>| {
        let sym = ctx.symbols.get_or_intern(name);
        let frag = KfxFragment::new(
            KfxSymbol::NavContainer,
            name,
            IonValue::Struct(vec![
                (KfxSymbol::NavType as u64, IonValue::Symbol(nav_type as u64)),
                (KfxSymbol::NavContainerName as u64, IonValue::Symbol(sym)),
                (KfxSymbol::Entries as u64, IonValue::List(entries)),
            ]),
        );
        (sym, frag)
    };

    let mut fragments = Vec::new();
    let mut container_syms = Vec::new();

    // page_list first (Amazon's order): page-number nav, one flat entry per page.
    let page_entries = build_pdf_page_list_entries(&pdf.page_labels, recs);
    if !page_entries.is_empty() {
        let (sym, frag) = make_container("npag", KfxSymbol::PageList, page_entries);
        container_syms.push(sym);
        fragments.push(frag);
    }

    // toc second: nested chapter navigation, only when the PDF has bookmarks.
    let toc_entries = build_pdf_toc_entries(&pdf.outline, recs);
    if !toc_entries.is_empty() {
        let (sym, frag) = make_container("ntoc", KfxSymbol::Toc, toc_entries);
        container_syms.push(sym);
        fragments.push(frag);
    }

    if container_syms.is_empty() {
        return Vec::new();
    }

    fragments.push(KfxFragment::singleton(
        KfxSymbol::BookNavigation,
        IonValue::List(vec![IonValue::Struct(vec![
            (
                KfxSymbol::ReadingOrderName as u64,
                IonValue::Symbol(KfxSymbol::Default as u64),
            ),
            (
                KfxSymbol::NavContainers as u64,
                IonValue::List(container_syms.into_iter().map(IonValue::Symbol).collect()),
            ),
        ])]),
    ));
    fragments
}

/// Flat `page_list` entries — one per page,
/// `{representation:{label}, target_position:{id, offset}}`. The label is the
/// PDF's page label; the target is the page's image EID.
#[cfg(feature = "pdf")]
fn build_pdf_page_list_entries(labels: &[String], recs: &[PdfPageRec]) -> Vec<IonValue> {
    recs.iter()
        .enumerate()
        .map(|(i, rec)| {
            let label = labels
                .get(i)
                .cloned()
                .unwrap_or_else(|| (i + 1).to_string());
            IonValue::Struct(vec![
                (
                    KfxSymbol::Representation as u64,
                    IonValue::Struct(vec![(KfxSymbol::Label as u64, IonValue::String(label))]),
                ),
                (
                    KfxSymbol::TargetPosition as u64,
                    IonValue::Struct(vec![
                        (KfxSymbol::Id as u64, IonValue::Int(rec.image_id as i64)),
                        (KfxSymbol::Offset as u64, IonValue::Int(0)),
                    ]),
                ),
            ])
        })
        .collect()
}

/// Convert resolved outline items into nested KFX TOC entries — plain
/// `{representation:{label}, target_position:{id, offset}, [entries]}` structs.
/// `target_position.id` is the page's image EID; an out-of-range page is skipped.
#[cfg(feature = "pdf")]
fn build_pdf_toc_entries(
    items: &[crate::import::pdf::PdfOutlineItem],
    recs: &[PdfPageRec],
) -> Vec<IonValue> {
    items
        .iter()
        .filter_map(|item| {
            let rec = recs.get(item.page_index)?;
            let mut fields = vec![
                (
                    KfxSymbol::Representation as u64,
                    IonValue::Struct(vec![(
                        KfxSymbol::Label as u64,
                        IonValue::String(item.title.clone()),
                    )]),
                ),
                (
                    KfxSymbol::TargetPosition as u64,
                    IonValue::Struct(vec![
                        (KfxSymbol::Id as u64, IonValue::Int(rec.image_id as i64)),
                        (KfxSymbol::Offset as u64, IonValue::Int(0)),
                    ]),
                ),
            ];
            if !item.children.is_empty() {
                let kids = build_pdf_toc_entries(&item.children, recs);
                if !kids.is_empty() {
                    fields.push((KfxSymbol::Entries as u64, IonValue::List(kids)));
                }
            }
            Some(IonValue::Struct(fields))
        })
        .collect()
}

/// container_entity_map ($419): the entity list plus the dependency graph
/// section → external_resource → shared bcRawMedia. Every page's
/// external_resource depends on the single shared raw-media location.
#[cfg(feature = "pdf")]
fn build_pdf_container_entity_map_fragment(
    container_id: &str,
    fragments: &[KfxFragment],
    recs: &[PdfPageRec],
    raw_location: &str,
    cover_dep: Option<(u64, &str)>,
    ctx: &ExportContext,
) -> KfxFragment {
    let mut entity_names: Vec<IonValue> = Vec::new();
    for frag in fragments {
        if frag.fid.starts_with('$') {
            continue;
        }
        if let Some(sym) = ctx.symbols.get(&frag.fid) {
            entity_names.push(IonValue::Symbol(sym));
        }
    }
    let container_entry = IonValue::Struct(vec![
        (
            KfxSymbol::Id as u64,
            IonValue::String(container_id.to_string()),
        ),
        (KfxSymbol::Contains as u64, IonValue::List(entity_names)),
    ]);

    let Some(raw_sym) = ctx.symbols.get(raw_location) else {
        // Should never happen — interned in the survey pass.
        let ion = IonValue::Struct(vec![(
            KfxSymbol::ContainerList as u64,
            IonValue::List(vec![container_entry]),
        )]);
        return KfxFragment::container_entity_map(ion);
    };

    let mut dependencies: Vec<IonValue> = Vec::with_capacity(recs.len() * 2);
    for rec in recs {
        // section → [external_resource]
        dependencies.push(IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Symbol(rec.section_sym)),
            (
                KfxSymbol::MandatoryDependencies as u64,
                IonValue::List(vec![IonValue::Symbol(rec.res_sym)]),
            ),
        ]));
        // external_resource → [shared bcRawMedia]
        dependencies.push(IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Symbol(rec.res_sym)),
            (
                KfxSymbol::MandatoryDependencies as u64,
                IonValue::List(vec![IonValue::Symbol(raw_sym)]),
            ),
        ]));
    }

    // cover external_resource → [cover bcRawMedia], when a cover is present.
    if let Some((cover_res_sym, cover_location)) = cover_dep
        && let Some(cover_media_sym) = ctx.symbols.get(cover_location)
    {
        dependencies.push(IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Symbol(cover_res_sym)),
            (
                KfxSymbol::MandatoryDependencies as u64,
                IonValue::List(vec![IonValue::Symbol(cover_media_sym)]),
            ),
        ]));
    }

    let ion = IonValue::Struct(vec![
        (
            KfxSymbol::ContainerList as u64,
            IonValue::List(vec![container_entry]),
        ),
        (
            KfxSymbol::EntityDependencies as u64,
            IonValue::List(dependencies),
        ),
    ]);
    KfxFragment::container_entity_map(ion)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_symbol_table_ion() {
        let symbols = vec!["section-1".to_string(), "section-2".to_string()];
        let ion = build_symbol_table_ion(&symbols);

        // Should start with Ion BVM
        assert_eq!(&ion[..4], &[0xe0, 0x01, 0x00, 0xea]);
    }

    #[test]
    fn test_build_format_capabilities_ion() {
        let ion = build_format_capabilities_ion();

        // Should start with Ion BVM
        assert_eq!(&ion[..4], &[0xe0, 0x01, 0x00, 0xea]);
    }

    #[test]
    fn test_metadata_fragment_contains_reading_orders() {
        let mut ctx = ExportContext::new();
        // Register some sections
        ctx.register_section("c0");
        ctx.register_section("c1");

        let meta = crate::model::Metadata::default();
        let frag = build_metadata_fragment(&meta, &ctx);

        // Should be $258 (metadata) type
        assert_eq!(frag.ftype, KfxSymbol::Metadata as u64);
        assert!(frag.is_singleton());

        // Extract Ion and verify structure
        if let crate::formats::kfx::fragment::FragmentData::Ion(ion) = &frag.data {
            if let IonValue::Struct(fields) = ion {
                // Should have reading_orders field
                let has_reading_orders = fields
                    .iter()
                    .any(|(id, _)| *id == KfxSymbol::ReadingOrders as u64);
                assert!(has_reading_orders, "metadata should contain reading_orders");
            } else {
                panic!("expected Struct");
            }
        } else {
            panic!("expected Ion data");
        }
    }

    #[test]
    fn test_book_metadata_fragment_has_categorised_metadata() {
        // Load a real book from fixtures
        let book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let ctx = ExportContext::new();
        let container_id = generate_container_id("test");

        let frag = build_book_metadata_fragment(&book, &container_id, &ctx);

        // Should be $490 (book_metadata) type
        assert_eq!(frag.ftype, KfxSymbol::BookMetadata as u64);
        assert!(frag.is_singleton());

        // Extract Ion and verify structure
        if let crate::formats::kfx::fragment::FragmentData::Ion(ion) = &frag.data {
            if let IonValue::Struct(fields) = ion {
                // Should have categorised_metadata field
                let has_categorised = fields
                    .iter()
                    .any(|(id, _)| *id == KfxSymbol::CategorisedMetadata as u64);
                assert!(
                    has_categorised,
                    "book_metadata should contain categorised_metadata"
                );

                // Get the categorised_metadata list
                let categorised = fields
                    .iter()
                    .find(|(id, _)| *id == KfxSymbol::CategorisedMetadata as u64)
                    .map(|(_, v)| v);

                if let Some(IonValue::List(categories)) = categorised {
                    // Should have 4 categories: ebook, title, audit, capability
                    // (capability is empty but its presence appears required
                    // for the device library cover extractor).
                    assert_eq!(categories.len(), 4, "should have 4 metadata categories");
                } else {
                    panic!("categorised_metadata should be a list");
                }
            } else {
                panic!("expected Struct");
            }
        } else {
            panic!("expected Ion data");
        }
    }

    #[test]
    fn test_metadata_kv_helper() {
        let kv = metadata_kv(
            "test_key",
            &crate::formats::kfx::metadata::MetadataValue::Text("test_value".to_string()),
        );

        if let IonValue::Struct(fields) = kv {
            assert_eq!(fields.len(), 2);

            let key_field = fields.iter().find(|(id, _)| *id == KfxSymbol::Key as u64);
            let value_field = fields.iter().find(|(id, _)| *id == KfxSymbol::Value as u64);

            assert!(key_field.is_some(), "should have key field");
            assert!(value_field.is_some(), "should have value field");

            if let Some((_, IonValue::String(k))) = key_field {
                assert_eq!(k, "test_key");
            }
            if let Some((_, IonValue::String(v))) = value_field {
                assert_eq!(v, "test_value");
            }
        } else {
            panic!("expected Struct");
        }
    }

    #[test]
    fn test_book_navigation_structure() {
        // Test that navigation has correct wrapper structure:
        // [{reading_order_name: default, nav_containers: [nav_container::{}...]}]
        let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let mut ctx = ExportContext::new();

        // Collect spine info first to avoid borrow issues
        let spine_info: Vec<_> = book
            .spine()
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let section_name = format!("c{}", idx);
                let source_path = book.source_id(entry.id).unwrap_or("").to_string();
                (entry.id, section_name, source_path)
            })
            .collect();

        // Survey chapters to populate path_to_fragment
        for (chapter_id, section_name, source_path) in &spine_info {
            ctx.register_section(section_name);
            if let Ok(chapter) = book.load_chapter(*chapter_id) {
                survey_chapter(&chapter, *chapter_id, source_path, &mut ctx);
            }
        }

        let frag = build_book_navigation_fragment_with_positions(&book, &ctx);

        // Should be $389 (book_navigation) type
        assert_eq!(frag.ftype, KfxSymbol::BookNavigation as u64);

        if let crate::formats::kfx::fragment::FragmentData::Ion(ion) = &frag.data {
            // Should be a list with one reading order entry
            if let IonValue::List(reading_orders) = ion {
                assert_eq!(reading_orders.len(), 1, "should have one reading order");

                // The reading order should have reading_order_name and nav_containers
                if let IonValue::Struct(fields) = &reading_orders[0] {
                    let has_reading_order_name = fields
                        .iter()
                        .any(|(id, _)| *id == KfxSymbol::ReadingOrderName as u64);
                    let has_nav_containers = fields
                        .iter()
                        .any(|(id, _)| *id == KfxSymbol::NavContainers as u64);

                    assert!(has_reading_order_name, "should have reading_order_name");
                    assert!(has_nav_containers, "should have nav_containers");
                } else {
                    panic!("reading order should be a struct");
                }
            } else {
                panic!("book_navigation should be a list");
            }
        } else {
            panic!("expected Ion data");
        }
    }

    /// The synthesized cover TOC entry targets the cover section root — the id
    /// the `cover_page` landmark uses, which the device merges with it into one
    /// jumpable 表紙. A content eid gets the entry dropped on-device.
    #[test]
    fn cover_toc_entry_targets_section_root_matching_landmark() {
        let book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let first_chapter = book.spine()[0].id;
        const SECTION_ROOT: u64 = 866;

        let mut ctx = ExportContext::new();
        // Cover landmark left at the section root (as the real pipeline leaves
        // it — exempt from fix_landmark_content_ids).
        ctx.landmark_fragments.insert(
            LandmarkType::Cover,
            LandmarkTarget {
                fragment_id: SECTION_ROOT,
                offset: 0,
                label: "Cover".to_string(),
            },
        );
        ctx.chapter_fragments.insert(first_chapter, SECTION_ROOT);
        ctx.content_ids_by_chapter
            .entry(first_chapter)
            .or_default()
            .push(881);

        let entry = build_cover_toc_entry(&book, &ctx).expect("cover entry");
        let IonValue::Annotated(_, boxed) = &entry else {
            panic!("nav_unit should be annotated");
        };
        let IonValue::Struct(fields) = boxed.as_ref() else {
            panic!("expected struct");
        };
        let tp = fields
            .iter()
            .find(|(k, _)| *k == KfxSymbol::TargetPosition as u64)
            .map(|(_, v)| v)
            .expect("target_position");
        let IonValue::Struct(tp_fields) = tp else {
            panic!("target_position should be a struct");
        };
        let id = tp_fields
            .iter()
            .find(|(k, _)| *k == KfxSymbol::Id as u64)
            .and_then(|(_, v)| match v {
                IonValue::Int(n) => Some(*n),
                _ => None,
            })
            .expect("id");
        assert_eq!(
            id, SECTION_ROOT as i64,
            "cover TOC must target the section root, matching the landmark"
        );
    }

    /// The feature keys a content_features fragment declares.
    fn feature_keys(frag: &KfxFragment) -> Vec<String> {
        let FragmentData::Ion(IonValue::Struct(fields)) = &frag.data else {
            panic!("expected Ion struct");
        };
        let Some((_, IonValue::List(items))) = fields
            .iter()
            .find(|(id, _)| *id == KfxSymbol::Features as u64)
        else {
            panic!("content_features should contain a features list");
        };
        items
            .iter()
            .filter_map(|item| {
                item.as_struct()
                    .and_then(|f| get_field(f, KfxSymbol::Key as u64))
                    .and_then(|v| v.as_string())
                    .map(str::to_string)
            })
            .collect()
    }

    #[test]
    fn test_content_features_fragment() {
        let ctx = ExportContext::new();
        let frag = build_content_features_fragment(&ctx, ContentFacts::default());

        assert_eq!(frag.ftype, KfxSymbol::ContentFeatures as u64);
        assert!(frag.is_singleton());
        assert_eq!(feature_keys(&frag), ["reflow-style", "CanonicalFormat"]);
    }

    /// A book with no images and no long section claims nothing about media.
    /// `yj_hdv` covers tiled imagery this writer never emits.
    #[test]
    fn a_plain_book_declares_no_media_features() {
        let ctx = ExportContext::new();
        let keys = feature_keys(&build_content_features_fragment(
            &ctx,
            ContentFacts::default(),
        ));
        for claim in [
            "yj_hdv",
            "yj_jpegxr_sd",
            "yj_jpg_rst_marker_present",
            "reflow-section-size",
        ] {
            assert!(
                !keys.contains(&claim.to_string()),
                "a plain book must not declare {claim}"
            );
        }
    }

    /// `yj_mixed_writing_mode` announces that the two axes coexist — what a
    /// horizontally-set document carrying vertical runs states. A vertical
    /// document states it through `document_data.writing_mode`.
    #[test]
    fn mixed_writing_mode_follows_a_horizontal_document_over_vertical_content() {
        let mut ctx = ExportContext::new();
        ctx.content_language = "ja".to_string();

        for (document, baseline, expected) in [
            (KfxSymbol::HorizontalTb, KfxSymbol::VerticalRl, true),
            (KfxSymbol::HorizontalTb, KfxSymbol::HorizontalTb, false),
            (KfxSymbol::VerticalRl, KfxSymbol::VerticalRl, false),
            (KfxSymbol::VerticalRl, KfxSymbol::HorizontalTb, false),
        ] {
            ctx.document_writing_mode = document;
            ctx.style_writing_mode_baseline = baseline;
            let keys = feature_keys(&build_content_features_fragment(
                &ctx,
                ContentFacts::default(),
            ));
            assert_eq!(
                keys.contains(&"yj_mixed_writing_mode".to_string()),
                expected,
                "document {document:?} over baseline {baseline:?}"
            );
        }
    }

    /// The Japanese vertical marker tracks vertical runs, not the document
    /// default: a book forced horizontal over vertical content carries it.
    #[test]
    fn the_jp_vertical_marker_follows_vertical_content() {
        let mut ctx = ExportContext::new();
        ctx.content_language = "ja".to_string();
        ctx.document_writing_mode = KfxSymbol::HorizontalTb;
        ctx.style_writing_mode_baseline = KfxSymbol::VerticalRl;

        let keys = feature_keys(&build_content_features_fragment(
            &ctx,
            ContentFacts::default(),
        ));
        assert!(keys.contains(&"jp-reflow-language".to_string()));
        assert!(keys.contains(&"jpvertical-reflow-language".to_string()));
    }

    #[test]
    fn jpeg_xr_plates_are_declared() {
        let ctx = ExportContext::new();
        let facts = ContentFacts {
            jxr_image: true,
            ..Default::default()
        };
        assert!(
            feature_keys(&build_content_features_fragment(&ctx, facts))
                .contains(&"yj_jpegxr_sd".to_string())
        );
    }

    #[test]
    fn restart_markers_are_declared() {
        let ctx = ExportContext::new();
        let facts = ContentFacts {
            jpeg_restart_markers: true,
            ..Default::default()
        };
        assert!(
            feature_keys(&build_content_features_fragment(&ctx, facts))
                .contains(&"yj_jpg_rst_marker_present".to_string())
        );
    }

    /// The declared size scales with how far the longest section overruns the
    /// renderer's 65536-position bound, and is absent below it.
    #[test]
    fn long_sections_declare_a_scaled_reflow_section_size() {
        assert_eq!(ContentFacts::default().reflow_section_size(), None);
        for (pids, expected) in [
            (65536, None),
            (65537, Some(2)),
            (65536 + 16384, Some(3)),
            (65536 + 7 * 16384, Some(9)),
            (i64::from(u32::MAX), Some(256)), // clamped
        ] {
            let facts = ContentFacts {
                max_section_pids: pids,
                ..Default::default()
            };
            assert_eq!(facts.reflow_section_size(), expected, "at {pids} positions");
        }
    }

    /// The facts are read back off the finished fragments: a resource any
    /// emission path adds is seen.
    #[test]
    fn content_facts_read_media_off_the_fragments() {
        let jxr = KfxFragment::new(
            KfxSymbol::ExternalResource,
            "e0",
            IonValue::Struct(vec![(
                KfxSymbol::Format as u64,
                IonValue::Symbol(KfxSymbol::Jxr as u64),
            )]),
        );
        // A JPEG carrying a restart marker (FF D0) after its SOI.
        let jpeg = KfxFragment::raw(
            KfxSymbol::Bcrawmedia,
            "resource/r0",
            vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0xFF, 0xD0, 0xFF, 0xD9],
        );
        let facts = ContentFacts::from_fragments(&[jxr, jpeg]);
        assert!(facts.jxr_image);
        assert!(facts.jpeg_restart_markers);

        // A font that happens to contain the same bytes is not a JPEG.
        let font = KfxFragment::raw(
            KfxSymbol::Bcrawmedia,
            "resource/f0",
            vec![0x00, 0x01, 0x00, 0x00, 0xFF, 0xD0],
        );
        assert!(!ContentFacts::from_fragments(&[font]).jpeg_restart_markers);
    }

    #[test]
    fn pick_document_writing_mode_any_vertical_beats_horizontal_majority() {
        // The LV999 case: 63 vertical-rl styles against 91 horizontal from
        // fixed-layout image pages. Vertical wins and the device turns
        // right-to-left.
        assert_eq!(
            pick_document_writing_mode(63, 0),
            KfxSymbol::VerticalRl,
            "vertical-rl text must win even when outnumbered by horizontal scaffolding"
        );
        // No vertical text at all → horizontal (the common English/LTR book).
        assert_eq!(pick_document_writing_mode(0, 0), KfxSymbol::HorizontalTb);
        // A predominantly vertical-lr book (rare) picks that axis.
        assert_eq!(pick_document_writing_mode(1, 5), KfxSymbol::VerticalLr);
        // vertical-rl wins ties against vertical-lr.
        assert_eq!(pick_document_writing_mode(4, 4), KfxSymbol::VerticalRl);
    }

    #[test]
    fn direction_is_rtl_only_for_horizontal_rtl_books() {
        // Vertical-rl rtl book: the writing-mode `-rl` override carries the
        // turn and direction stays ltr, matching Amazon.
        assert_eq!(
            direction_for_progression(true, KfxSymbol::VerticalRl),
            KfxSymbol::Ltr
        );
        // Horizontal rtl book (rtl manga): no `-rl` writing mode, and direction
        // carries the turn.
        assert_eq!(
            direction_for_progression(true, KfxSymbol::HorizontalTb),
            KfxSymbol::Rtl
        );
        // Vertical-lr rtl (contradictory/Mongolian) takes direction rtl.
        assert_eq!(
            direction_for_progression(true, KfxSymbol::VerticalLr),
            KfxSymbol::Rtl
        );
        // ltr books are always ltr regardless of writing mode.
        assert_eq!(
            direction_for_progression(false, KfxSymbol::HorizontalTb),
            KfxSymbol::Ltr
        );
        assert_eq!(
            direction_for_progression(false, KfxSymbol::VerticalRl),
            KfxSymbol::Ltr
        );
    }

    #[test]
    fn test_document_data_fragment() {
        let mut ctx = ExportContext::new();
        ctx.register_section("c0");
        ctx.register_section("c1");
        // Simulate some fragment IDs being used
        ctx.next_fragment_id();
        ctx.next_fragment_id();

        let frag = build_document_data_fragment(&ctx);

        // Should be $538 (document_data) type
        assert_eq!(frag.ftype, KfxSymbol::DocumentData as u64);
        assert!(frag.is_singleton());

        // Extract Ion and verify structure
        if let crate::formats::kfx::fragment::FragmentData::Ion(ion) = &frag.data {
            if let IonValue::Struct(fields) = ion {
                // Check for required fields
                let field_ids: Vec<u64> = fields.iter().map(|(id, _)| *id).collect();

                assert!(
                    field_ids.contains(&(KfxSymbol::Direction as u64)),
                    "should have direction"
                );
                assert!(
                    field_ids.contains(&(KfxSymbol::ColumnCount as u64)),
                    "should have column_count"
                );
                assert!(
                    field_ids.contains(&(KfxSymbol::FontSize as u64)),
                    "should have font_size"
                );
                assert!(
                    field_ids.contains(&(KfxSymbol::WritingMode as u64)),
                    "should have writing_mode"
                );
                assert!(
                    field_ids.contains(&(KfxSymbol::Selection as u64)),
                    "should have selection"
                );
                assert!(
                    field_ids.contains(&(KfxSymbol::MaxId as u64)),
                    "should have max_id"
                );
                assert!(
                    field_ids.contains(&(KfxSymbol::LineHeight as u64)),
                    "should have line_height"
                );
                assert!(
                    field_ids.contains(&(KfxSymbol::ReadingOrders as u64)),
                    "should have reading_orders"
                );
            } else {
                panic!("expected Struct");
            }
        } else {
            panic!("expected Ion data");
        }
    }

    #[test]
    fn test_document_data_max_id_reflects_all_fragment_ids() {
        let mut ctx = ExportContext::new();
        ctx.register_section("c0");

        // Simulate generating many fragment IDs (like content generation does)
        for _ in 0..100 {
            ctx.next_fragment_id();
        }

        let frag = build_document_data_fragment(&ctx);

        // Extract max_id from the fragment
        if let crate::formats::kfx::fragment::FragmentData::Ion(IonValue::Struct(fields)) =
            &frag.data
        {
            let max_id_field = fields.iter().find(|(id, _)| *id == KfxSymbol::MaxId as u64);

            if let Some((_, IonValue::Int(max_id))) = max_id_field {
                // max_id covers the 100 generated IDs; ExportContext starts at
                // 866, putting 100 IDs at 965
                assert!(
                    *max_id >= 100,
                    "max_id ({}) should reflect all generated fragment IDs",
                    max_id
                );
            } else {
                panic!("max_id should be an integer");
            }
        } else {
            panic!("expected Ion struct data");
        }
    }

    #[test]
    fn test_singleton_uses_null_symbol() {
        // Build a singleton fragment and serialize it
        let ctx = ExportContext::new();
        let frag = build_content_features_fragment(&ctx, ContentFacts::default());
        let local_symbols: Vec<String> = vec![];
        let entities = serialize_fragments(&[frag], &local_symbols);

        // Singleton should use $348 (null) as ID
        assert_eq!(entities[0].id, KfxSymbol::Null as u32);
    }

    #[test]
    fn test_build_headings_entries_empty() {
        let ctx = ExportContext::new();
        let entries = build_headings_entries(&ctx);
        assert!(
            entries.is_empty(),
            "No headings should produce empty entries"
        );
    }

    #[test]
    fn test_build_headings_entries_single_level() {
        use crate::formats::kfx::context::HeadingPosition;

        let mut ctx = ExportContext::new();

        // Push h2 headings at different positions
        ctx.heading_positions.push(HeadingPosition {
            level: 2,
            fragment_id: 100,
            offset: 0,
        });
        ctx.heading_positions.push(HeadingPosition {
            level: 2,
            fragment_id: 100,
            offset: 50,
        });
        ctx.heading_positions.push(HeadingPosition {
            level: 2,
            fragment_id: 101,
            offset: 0,
        });

        let entries = build_headings_entries(&ctx);

        // Should have 1 level entry (h2)
        assert_eq!(entries.len(), 1, "Should have one level group for h2");

        // Verify it's a nav_unit with h2 landmark_type
        if let IonValue::Annotated(annotations, inner) = &entries[0] {
            assert_eq!(annotations[0], KfxSymbol::NavUnit as u64);
            if let IonValue::Struct(fields) = inner.as_ref() {
                // Should have landmark_type = h2
                let landmark = fields
                    .iter()
                    .find(|(id, _)| *id == KfxSymbol::LandmarkType as u64);
                assert!(landmark.is_some(), "Should have landmark_type");
                if let Some((_, IonValue::Symbol(sym))) = landmark {
                    assert_eq!(*sym, KfxSymbol::H2 as u64);
                }

                // Should have nested entries
                let nested = fields
                    .iter()
                    .find(|(id, _)| *id == KfxSymbol::Entries as u64);
                assert!(nested.is_some(), "Should have nested entries");
                if let Some((_, IonValue::List(list))) = nested {
                    assert_eq!(list.len(), 3, "Should have 3 nested h2 entries");
                }
            }
        } else {
            panic!("Expected annotated nav_unit");
        }
    }

    #[test]
    fn test_build_headings_entries_multiple_levels() {
        use crate::formats::kfx::context::HeadingPosition;

        let mut ctx = ExportContext::new();

        // Push h2, h3, h4 headings
        ctx.heading_positions.push(HeadingPosition {
            level: 2,
            fragment_id: 100,
            offset: 0,
        });
        ctx.heading_positions.push(HeadingPosition {
            level: 3,
            fragment_id: 100,
            offset: 20,
        });
        ctx.heading_positions.push(HeadingPosition {
            level: 4,
            fragment_id: 101,
            offset: 0,
        });
        ctx.heading_positions.push(HeadingPosition {
            level: 3,
            fragment_id: 101,
            offset: 30,
        });

        let entries = build_headings_entries(&ctx);

        // Should have 3 level entries (h2, h3, h4)
        assert_eq!(entries.len(), 3, "Should have three level groups");

        // Verify ordering is by level (BTreeMap ensures h2 < h3 < h4)
        let levels: Vec<u64> = entries
            .iter()
            .filter_map(|e| {
                if let IonValue::Annotated(_, inner) = e {
                    if let IonValue::Struct(fields) = inner.as_ref() {
                        fields
                            .iter()
                            .find(|(id, _)| *id == KfxSymbol::LandmarkType as u64)
                            .and_then(|(_, v)| {
                                if let IonValue::Symbol(sym) = v {
                                    Some(*sym)
                                } else {
                                    None
                                }
                            })
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(
            levels,
            vec![
                KfxSymbol::H2 as u64,
                KfxSymbol::H3 as u64,
                KfxSymbol::H4 as u64
            ]
        );
    }

    #[test]
    fn test_build_headings_entries_ignores_h1() {
        use crate::formats::kfx::context::HeadingPosition;

        let mut ctx = ExportContext::new();

        ctx.heading_positions.push(HeadingPosition {
            level: 1,
            fragment_id: 100,
            offset: 0,
        });

        let entries = build_headings_entries(&ctx);
        assert!(entries.is_empty(), "h1 should be ignored");
    }

    #[test]
    fn test_build_headings_entries_target_position() {
        use crate::formats::kfx::context::HeadingPosition;

        let mut ctx = ExportContext::new();

        ctx.heading_positions.push(HeadingPosition {
            level: 2,
            fragment_id: 12345,
            offset: 99,
        });

        let entries = build_headings_entries(&ctx);
        assert_eq!(entries.len(), 1);

        // Verify target_position has correct id and offset
        if let IonValue::Annotated(_, inner) = &entries[0]
            && let IonValue::Struct(fields) = inner.as_ref()
        {
            let target = fields
                .iter()
                .find(|(id, _)| *id == KfxSymbol::TargetPosition as u64);
            if let Some((_, IonValue::Struct(pos_fields))) = target {
                let id_field = pos_fields
                    .iter()
                    .find(|(id, _)| *id == KfxSymbol::Id as u64);
                let offset_field = pos_fields
                    .iter()
                    .find(|(id, _)| *id == KfxSymbol::Offset as u64);

                if let Some((_, IonValue::Int(id))) = id_field {
                    assert_eq!(*id, 12345);
                } else {
                    panic!("Expected Int id");
                }

                if let Some((_, IonValue::Int(offset))) = offset_field {
                    assert_eq!(*offset, 99);
                } else {
                    panic!("Expected Int offset");
                }
            }
        }
    }

    #[test]
    fn position_maps_are_section_keyed_with_per_section_walk() {
        use crate::ChapterId;
        use crate::formats::kfx::fragment::FragmentData;

        let mut ctx = ExportContext::new();
        let c0 = ctx.register_section("c0");
        let c1 = ctx.register_section("c1");
        let chapter1 = ChapterId(1);
        let chapter2 = ChapterId(2);
        ctx.content_ids_by_chapter
            .entry(chapter1)
            .or_default()
            .extend(vec![100, 101, 102]);
        ctx.content_ids_by_chapter
            .entry(chapter2)
            .or_default()
            .extend(vec![200, 201]);
        // chapter_fragments = the section roots; no content_id_lengths ⇒ every
        // span defaults to 1.
        ctx.chapter_fragments.insert(chapter1, 90);
        ctx.chapter_fragments.insert(chapter2, 95);

        let names = vec!["c0".to_string(), "c1".to_string()];
        let secs = section_positions(&ctx, &names);

        // Each section = root (chapter_fragment) first, then its content, span 1.
        assert_eq!(secs.len(), 2);
        assert_eq!(secs[0].sym, c0);
        assert_eq!(secs[0].eids, vec![(90, 1), (100, 1), (101, 1), (102, 1)]);
        assert_eq!(secs[1].sym, c1);
        assert_eq!(secs[1].eids, vec![(95, 1), (200, 1), (201, 1)]);

        // position_id_map: section-keyed {section_name, pid, length}.
        let pidmap = build_position_id_map_fragment(&secs);
        let FragmentData::Ion(IonValue::Struct(top)) = &pidmap.data else {
            panic!("position_id_map should be a struct");
        };
        let IonValue::List(sec_entries) = top
            .iter()
            .find(|(k, _)| *k == KfxSymbol::Contains as u64)
            .map(|(_, v)| v)
            .expect("contains")
        else {
            panic!("contains should be a list");
        };
        let field = |s: &IonValue, key: KfxSymbol| -> i64 {
            let IonValue::Struct(f) = s else { panic!() };
            f.iter()
                .find(|(k, _)| *k == key as u64)
                .and_then(|(_, v)| match v {
                    IonValue::Int(n) => Some(*n),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(sec_entries.len(), 2);
        // section 0: pid 0, length 4; section 1: pid 4, length 3.
        assert_eq!(field(&sec_entries[0], KfxSymbol::Pid), 0);
        assert_eq!(field(&sec_entries[0], KfxSymbol::Length), 4);
        assert_eq!(field(&sec_entries[1], KfxSymbol::Pid), 4);
        assert_eq!(field(&sec_entries[1], KfxSymbol::Length), 3);

        // section_position_id_map: one entity per section, keyed by section name.
        let spm = build_section_position_id_map_fragments(&secs);
        assert_eq!(spm.len(), 2);
        assert!(
            spm.iter()
                .all(|f| f.ftype == KfxSymbol::SectionPositionIdMap as u64)
        );
        // Section 0 walk: [0,90] [1,100] 1 1 [1,0] — each element advances by the
        // PREVIOUS span; a bare int names previous+1, a pair names an explicit eid.
        let FragmentData::Ion(IonValue::Struct(s0)) = &spm[0].data else {
            panic!("section_position_id_map should be a struct");
        };
        let IonValue::List(walk) = s0
            .iter()
            .find(|(k, _)| *k == KfxSymbol::Contains as u64)
            .map(|(_, v)| v)
            .expect("contains")
        else {
            panic!("walk should be a list");
        };
        let expect_pair = |v: &IonValue, a: i64, e: i64| {
            let IonValue::List(p) = v else {
                panic!("expected [advance, eid]")
            };
            assert_eq!(p[0].as_int(), Some(a));
            assert_eq!(p[1].as_int(), Some(e));
        };
        assert_eq!(walk.len(), 5);
        expect_pair(&walk[0], 0, 90); // root: advance 0, explicit eid
        expect_pair(&walk[1], 1, 100); // advance 1 (root span), explicit (not 91)
        assert_eq!(walk[2].as_int(), Some(1)); // 101 == 100+1 → bare advance
        assert_eq!(walk[3].as_int(), Some(1)); // 102 == 101+1 → bare advance
        expect_pair(&walk[4], 1, 0); // terminator at pid == length
    }
}

#[cfg(test)]
#[allow(clippy::vec_init_then_push, clippy::needless_range_loop)]
mod entity_structure_tests {
    use super::*;
    use crate::formats::kfx::fragment::FragmentData;
    use crate::model::Book;

    #[test]
    fn test_entity_order_matches_reference() {
        // Build KFX from EPUB and verify entity order matches Amazon reference
        let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let container_id = generate_container_id("test");
        let mut ctx = ExportContext::new();

        // Collect spine info
        let spine_info: Vec<_> = book
            .spine()
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let section_name = format!("c{}", idx);
                (entry.id, section_name)
            })
            .collect();

        // Pass 1: Survey
        for (chapter_id, section_name) in &spine_info {
            ctx.register_section(section_name);
            let source_path = book.source_id(*chapter_id).unwrap_or("").to_string();
            if let Ok(chapter) = book.load_chapter(*chapter_id) {
                survey_chapter(&chapter, *chapter_id, &source_path, &mut ctx);
            }
        }

        // Pass 2: Build fragments in correct order
        let mut fragments = Vec::new();

        fragments.push(build_content_features_fragment(
            &ctx,
            ContentFacts::default(),
        ));
        fragments.push(build_book_metadata_fragment(&book, &container_id, &ctx));
        fragments.push(build_metadata_fragment(book.metadata(), &ctx));
        fragments.push(build_document_data_fragment(&ctx));
        fragments.push(build_book_navigation_fragment_with_positions(&book, &ctx));

        let mut section_fragments = Vec::new();
        let mut storyline_fragments = Vec::new();
        let mut content_fragments = Vec::new();

        for (chapter_id, section_name) in &spine_info {
            if let Ok(chapter) = book.load_chapter(*chapter_id) {
                let (section, storyline, content) =
                    build_chapter_entities_grouped(&chapter, *chapter_id, section_name, &mut ctx);
                section_fragments.push(section);
                storyline_fragments.push(storyline);
                if let Some(c) = content {
                    content_fragments.push(c);
                }
            }
        }

        fragments.extend(section_fragments);
        fragments.extend(storyline_fragments);
        fragments.extend(content_fragments);

        // Entity type order: content_features, book_metadata, metadata,
        // document_data, book_navigation, then grouped sections, storylines,
        // content

        let types: Vec<u64> = fragments.iter().map(|f| f.ftype).collect();

        // First 5 should be the header entities in order
        assert_eq!(types[0], KfxSymbol::ContentFeatures as u64);
        assert_eq!(types[1], KfxSymbol::BookMetadata as u64);
        assert_eq!(types[2], KfxSymbol::Metadata as u64);
        assert_eq!(types[3], KfxSymbol::DocumentData as u64);
        assert_eq!(types[4], KfxSymbol::BookNavigation as u64);

        // After header, all sections should come first, then storylines, then content
        let after_header = &types[5..];
        let section_count = after_header
            .iter()
            .take_while(|&&t| t == KfxSymbol::Section as u64)
            .count();
        assert!(section_count > 0, "should have sections after header");

        let after_sections = &after_header[section_count..];
        let storyline_count = after_sections
            .iter()
            .take_while(|&&t| t == KfxSymbol::Storyline as u64)
            .count();
        assert!(storyline_count > 0, "should have storylines after sections");

        let after_storylines = &after_sections[storyline_count..];
        let content_count = after_storylines
            .iter()
            .take_while(|&&t| t == KfxSymbol::Content as u64)
            .count();
        // Content is optional (image-only chapters may not have content)
        // After storylines, every remaining entity is content
        for t in after_storylines.iter().take(content_count) {
            assert_eq!(
                *t,
                KfxSymbol::Content as u64,
                "content should follow storylines"
            );
        }

        // Verify grouping - no interleaving
        for i in 1..section_count {
            assert_eq!(
                after_header[i],
                KfxSymbol::Section as u64,
                "sections should be grouped"
            );
        }
        for i in 1..storyline_count {
            assert_eq!(
                after_sections[i],
                KfxSymbol::Storyline as u64,
                "storylines should be grouped"
            );
        }
    }

    #[test]
    fn test_chapter_entities_grouped_returns_correct_types() {
        let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let mut ctx = ExportContext::new();

        // Get first chapter
        let spine_entry = book.spine().first().unwrap();
        let chapter_id = spine_entry.id;
        let section_name = "c0";
        ctx.register_section(section_name);

        // Survey chapter first
        let source_path = book.source_id(chapter_id).unwrap_or("").to_string();
        if let Ok(chapter) = book.load_chapter(chapter_id) {
            survey_chapter(&chapter, chapter_id, &source_path, &mut ctx);
        }

        // Build entities
        let chapter = book.load_chapter(chapter_id).unwrap();
        let (section, storyline, content) =
            build_chapter_entities_grouped(&chapter, chapter_id, section_name, &mut ctx);

        // Verify types
        assert_eq!(section.ftype, KfxSymbol::Section as u64);
        assert_eq!(storyline.ftype, KfxSymbol::Storyline as u64);

        // Verify section has section_name and page_templates
        if let FragmentData::Ion(IonValue::Struct(fields)) = &section.data {
            let has_section_name = fields
                .iter()
                .any(|(id, _)| *id == KfxSymbol::SectionName as u64);
            let has_page_templates = fields
                .iter()
                .any(|(id, _)| *id == KfxSymbol::PageTemplates as u64);
            assert!(has_section_name, "section should have section_name");
            assert!(has_page_templates, "section should have page_templates");
        }

        // Verify storyline has story_name and content_list
        if let FragmentData::Ion(IonValue::Struct(fields)) = &storyline.data {
            let has_story_name = fields
                .iter()
                .any(|(id, _)| *id == KfxSymbol::StoryName as u64);
            let has_content_list = fields
                .iter()
                .any(|(id, _)| *id == KfxSymbol::ContentList as u64);
            assert!(has_story_name, "storyline should have story_name");
            assert!(has_content_list, "storyline should have content_list");
        }

        // Content is optional but if present should have name and content_list
        if let Some(content_frag) = content {
            assert_eq!(content_frag.ftype, KfxSymbol::Content as u64);
            if let FragmentData::Ion(IonValue::Struct(fields)) = &content_frag.data {
                let has_name = fields.iter().any(|(id, _)| *id == KfxSymbol::Name as u64);
                let has_content_list = fields
                    .iter()
                    .any(|(id, _)| *id == KfxSymbol::ContentList as u64);
                assert!(has_name, "content should have name");
                assert!(has_content_list, "content should have content_list");
            }
        }
    }
}

// A titlepage section takes type:text (not type:container) beside a standalone
// cover section, which needs a book whose cover image differs from its titlepage
// image. No fixture here has that pair.

#[cfg(test)]
mod page_background_tests {
    use super::*;
    use crate::style::{BackgroundSize, ComputedStyle};

    /// A picture `<body>` paints rides on the section's page template: the
    /// storyline walk emits the root's children, never the root. Anything else
    /// about body's box stays off it.
    #[test]
    fn body_background_becomes_a_page_style() {
        let mut chapter = Chapter::new();
        let style = ComputedStyle {
            background_image: Some("OEBPS/images/paper.jpg".to_string()),
            background_size: BackgroundSize::Cover,
            // Body's box, which must not follow the picture onto the page.
            margin_top: crate::style::Length::Px(40.0),
            ..Default::default()
        };
        let id = chapter.styles.intern(style);
        chapter.node_mut(chapter.root()).unwrap().style = id;

        let page = page_background_style(&chapter).expect("a painted body yields a page style");
        assert_eq!(
            page.background_image.as_deref(),
            Some("OEBPS/images/paper.jpg")
        );
        assert_eq!(page.background_size, BackgroundSize::Cover);
        assert_eq!(page.margin_top, crate::style::Length::Auto);
    }

    #[test]
    fn an_unpainted_body_leaves_the_page_template_alone() {
        let mut chapter = Chapter::new();
        let id = chapter.styles.intern(ComputedStyle {
            margin_top: crate::style::Length::Px(40.0),
            ..Default::default()
        });
        chapter.node_mut(chapter.root()).unwrap().style = id;
        assert!(page_background_style(&chapter).is_none());
    }
}

#[cfg(test)]
mod resource_export_tests {
    use super::*;
    use crate::model::Book;

    #[test]
    fn test_kfx_export_includes_images() {
        let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let data = build_kfx_container(&mut book, &|_, _, _, _| {}).unwrap();

        // 人間失格 is a text novel with one ~32KB cover image; the full KFX is
        // ~330KB. Assert it's substantial (text + bundled image), not empty.
        assert!(
            data.len() > 200_000,
            "KFX should include text + image data, got {} bytes",
            data.len()
        );
    }

    #[test]
    fn export_with_progress_emits_phases_in_order() {
        use std::cell::RefCell;
        let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let phases = RefCell::new(Vec::<String>::new());
        let mut sink = Vec::new();
        KfxExporter::new()
            .export_with_progress(
                &mut book,
                &mut std::io::Cursor::new(&mut sink),
                &|phase, cur, total, _label| {
                    // First sighting of each phase, in emission order. Counts must be sane.
                    assert!(cur >= 1 && cur <= total, "{phase}: {cur}/{total}");
                    let mut p = phases.borrow_mut();
                    if p.last().map(String::as_str) != Some(phase) {
                        p.push(phase.to_string());
                    }
                },
            )
            .unwrap();
        let seen = phases.into_inner();
        // The pipeline runs survey → chapters → images → finalize; the fixture
        // has a cover, and all four fire.
        let order = ["survey", "chapters", "images", "finalize"];
        let idxs: Vec<usize> = order
            .iter()
            .map(|p| {
                seen.iter()
                    .position(|s| s == p)
                    .unwrap_or_else(|| panic!("missing phase {p}; saw {seen:?}"))
            })
            .collect();
        assert!(
            idxs.windows(2).all(|w| w[0] < w[1]),
            "phases out of order: {seen:?}"
        );
        assert!(!sink.is_empty(), "should have written KFX bytes");
    }

    #[test]
    fn test_kfx_cover_jpeg_interiors_jxr() {
        // EPUB→KFX re-encodes interior raster plates as grayscale JXR and keeps
        // the COVER as JPEG — Amazon's own shape, and what the Kindle
        // library-gallery / sleep-screen thumbnailer reads.
        let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let kfx = build_kfx_container(&mut book, &|_, _, _, _| {}).unwrap();
        let loaded = crate::formats::kfx::loader::load(&kfx).expect("load own kfx");
        let is_jxr = |v: &Vec<u8>| v.len() >= 3 && v[0] == 0x49 && v[1] == 0x49 && v[2] == 0xBC;
        let is_jpeg = |v: &Vec<u8>| v.len() >= 3 && v[0] == 0xFF && v[1] == 0xD8 && v[2] == 0xFF;
        // 人間失格 is a text novel — its one image is the cover, which stays JPEG.
        assert!(
            loaded.raw_media.values().filter(|v| is_jpeg(v)).count() >= 1,
            "cover should stay JPEG for the thumbnailer"
        );
        assert_eq!(
            loaded.raw_media.values().filter(|v| is_jxr(v)).count(),
            0,
            "a cover-only book has no JXR interiors"
        );

        // And the interior-plate path really emits JXR (II-BC magic).
        let img: ::image::GrayImage =
            ::image::ImageBuffer::from_fn(32, 32, |x, _| ::image::Luma([(x * 8) as u8]));
        let mut png = std::io::Cursor::new(Vec::new());
        ::image::DynamicImage::ImageLuma8(img)
            .write_to(&mut png, ::image::ImageFormat::Png)
            .unwrap();
        let jxr = encode_jxr_asset(png.get_ref(), jxr::ColorMode::Grayscale)
            .expect("interior plate → JXR");
        assert_eq!(
            &jxr[0..3],
            &[0x49, 0x49, 0xBC],
            "interior plate must be JXR"
        );
        // The plate's fixed-layout page is sized from these dims; if unreadable
        // the device letterboxes it (margins). Must round-trip through the IFD.
        assert_eq!(
            crate::util::extract_image_dimensions(&jxr),
            Some((32, 32)),
            "JXR plate dimensions must be readable for full-bleed page sizing"
        );
    }

    #[test]
    fn encode_jxr_asset_honors_color_mode() {
        use jxr::ColorMode;
        // A genuinely-colorful plate, distinct R/G/B against the auto-gray path
        let rgb: ::image::RgbImage = ::image::ImageBuffer::from_fn(32, 32, |x, y| {
            ::image::Rgb([(x * 8) as u8, (y * 8) as u8, ((x + y) * 4) as u8])
        });
        let mut png = std::io::Cursor::new(Vec::new());
        ::image::DynamicImage::ImageRgb8(rgb)
            .write_to(&mut png, ::image::ImageFormat::Png)
            .unwrap();
        let dec = |bytes: &[u8]| -> (String, usize) {
            let c = jxr::decode::container::parse(bytes).unwrap();
            let n = jxr::decode::decoder::Decoder::new(c.image_data)
                .decode()
                .unwrap()
                .num_components;
            (c.pixel_format_uuid, n)
        };
        // Grayscale mode → 8bppGray (1 component).
        let g = encode_jxr_asset(png.get_ref(), ColorMode::Grayscale).unwrap();
        assert_eq!(&g[0..3], &[0x49, 0x49, 0xBC]);
        let (g_uuid, g_nc) = dec(&g);
        assert_eq!(g_nc, 1, "grayscale mode → 1 component");
        assert!(
            g_uuid.ends_with("dc908"),
            "grayscale → 8bppGray UUID, got {g_uuid}"
        );
        // Color mode → 24bppRGB (3 components).
        let c = encode_jxr_asset(png.get_ref(), ColorMode::Color).unwrap();
        assert_eq!(&c[0..3], &[0x49, 0x49, 0xBC]);
        let (c_uuid, c_nc) = dec(&c);
        assert_eq!(c_nc, 3, "color mode → 3 components");
        assert!(
            c_uuid.ends_with("dc90d"),
            "color → 24bppRGB UUID, got {c_uuid}"
        );
    }

    /// Decode a grayscale JXR plate into (width, height, luma bytes).
    fn decode_gray_jxr(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let c = jxr::decode::container::parse(bytes).unwrap();
        let d = jxr::decode::decoder::Decoder::new(c.image_data)
            .decode()
            .unwrap();
        let buf = d.to_pixel_buffer().unwrap();
        assert_eq!(buf.channels, 1, "expected a single luma plane");
        (d.width, d.height, buf.data)
    }

    #[test]
    fn transparent_raster_flattens_white_in_jxr() {
        // 32×32 RGBA PNG: an opaque black 8×8 square top-left, the rest fully
        // transparent black. Without alpha flattening `to_luma8` maps the
        // transparent region to luma 0 and the plate renders as a black slab.
        let rgba: ::image::RgbaImage = ::image::ImageBuffer::from_fn(32, 32, |x, y| {
            if x < 8 && y < 8 {
                ::image::Rgba([0, 0, 0, 255])
            } else {
                ::image::Rgba([0, 0, 0, 0])
            }
        });
        let mut png = std::io::Cursor::new(Vec::new());
        ::image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut png, ::image::ImageFormat::Png)
            .unwrap();
        let out = encode_jxr_asset(png.get_ref(), jxr::ColorMode::Grayscale).unwrap();
        let (w, _h, luma) = decode_gray_jxr(&out);
        let px = |x: u32, y: u32| luma[(y * w + x) as usize];
        // Sample away from the square's edge (JXR is lossy; avoid ringing).
        assert!(
            px(24, 24) >= 240,
            "transparent background must flatten to white, got {}",
            px(24, 24)
        );
        assert!(
            px(2, 2) <= 15,
            "opaque content stays black, got {}",
            px(2, 2)
        );
    }

    #[cfg(feature = "svg")]
    #[test]
    fn svg_asset_rasterizes_to_jxr_plate() {
        // 20×10 CSS px SVG, left half black on a transparent background.
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10" viewBox="0 0 20 10"><rect x="0" y="0" width="10" height="10" fill="black"/></svg>"#;
        let out = encode_asset_for_kfx(svg, jxr::ColorMode::Grayscale);
        assert_eq!(
            &out[0..3],
            &[0x49, 0x49, 0xBC],
            "svg must become a JXR plate, not raw XML labeled jpg"
        );
        // 4× supersample of the intrinsic CSS size, dims readable from the IFD.
        assert_eq!(crate::util::extract_image_dimensions(&out), Some((80, 40)));
        let (w, _h, luma) = decode_gray_jxr(&out);
        let px = |x: u32, y: u32| luma[(y * w + x) as usize];
        assert!(
            px(20, 20) <= 15,
            "painted rect is black, got {}",
            px(20, 20)
        );
        assert!(
            px(60, 20) >= 240,
            "transparent background flattens to white, got {}",
            px(60, 20)
        );
    }

    #[cfg(not(feature = "svg"))]
    #[test]
    fn svg_asset_is_refused_without_a_rasterizer() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"></svg>"#;
        let err = reject_unrasterizable_svg("art.svg", svg).expect_err("svg must be refused");
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        reject_unrasterizable_svg("photo.jpg", &[0xFF, 0xD8, 0xFF]).expect("a JPEG still passes");
    }

    #[cfg(feature = "svg")]
    #[test]
    fn svg_cover_rasterizes_to_jpeg() {
        // 20×10 CSS px SVG, left half black on a transparent background.
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10" viewBox="0 0 20 10"><rect x="0" y="0" width="10" height="10" fill="black"/></svg>"#;
        let out = cover_jpeg_for_kfx(svg).expect("svg cover encodes");
        assert_eq!(
            &out[0..3],
            &[0xFF, 0xD8, 0xFF],
            "cover must become a JPEG, not raw XML the device cannot render"
        );
        assert_eq!(crate::util::extract_image_dimensions(&out), Some((80, 40)));
    }

    #[test]
    fn test_kfx_asset_roundtrip() {
        // Export EPUB to KFX
        let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let kfx_data = build_kfx_container(&mut book, &|_, _, _, _| {}).unwrap();

        // Write to temp file and re-open
        let temp_path = std::env::temp_dir().join("test_roundtrip.kfx");
        std::fs::write(&temp_path, &kfx_data).unwrap();

        let mut reimported = Book::open(&temp_path).unwrap();
        let assets: Vec<_> = reimported.list_assets().to_vec();

        // Load all assets and verify total size
        let total_size: usize = assets
            .iter()
            .filter_map(|a| reimported.load_asset(a).ok())
            .map(|d| d.len())
            .sum();

        std::fs::remove_file(&temp_path).ok();

        // 人間失格 has a single ~32KB cover image; it must survive the roundtrip.
        assert!(
            total_size > 30_000,
            "Expected the cover image (~32KB) among KFX assets, got {} bytes",
            total_size
        );
    }
}

#[cfg(test)]
mod anchor_resolution_tests {
    use super::*;
    use crate::model::Book;

    #[test]
    fn test_cross_file_anchor_resolution_flow() {
        // Full anchor resolution on 人間失格.epub, whose TOC links enter the body
        let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();

        // Step 1: Resolve all links using centralized resolver
        let resolved = book.resolve_links().unwrap();

        // Should have resolved links (the TOC targets resolve internally)
        assert!(!resolved.is_empty(), "Should have resolved some links");

        // Check for some broken links (external links won't resolve)
        // but internal endnote links should resolve
        let broken_count = resolved.broken_links().len();
        eprintln!("Resolved {} links, {} broken", resolved.len(), broken_count);
    }

    #[test]
    fn test_anchor_symbol_reuse() {
        // Anchor symbols agree between link_to and anchor creation
        let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();

        let mut ctx = ExportContext::new();

        // Collect spine info
        let spine_info: Vec<_> = book
            .spine()
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let section_name = format!("c{}", idx);
                (entry.id, section_name)
            })
            .collect();

        // Step 1: Resolve links
        let resolved = book.resolve_links().unwrap();

        // Step 2: Register link targets from ResolvedLinks
        register_link_targets(&mut book, &spine_info, &resolved, &mut ctx).unwrap();

        // Step 3: Verify that href lookups return the same symbol as GlobalNodeId lookups
        // Find an internal link that has both
        for (source, target) in resolved.iter() {
            if let AnchorTarget::Internal(gid) = target {
                // Get the href for this link
                if let Ok(chapter) = book.load_chapter(source.chapter)
                    && let Some(href) = chapter.semantics.href(source.node)
                {
                    // Both lookups should return the same symbol
                    let href_symbol = ctx.anchor_registry.get_href_symbol(href);
                    let node_symbol = ctx.anchor_registry.get_symbol(*gid);

                    assert_eq!(
                        href_symbol, node_symbol,
                        "href '{}' and GlobalNodeId {:?} should have same symbol",
                        href, gid
                    );

                    // Only need to verify one link
                    return;
                }
            }
        }

        // Reaching here means no internal link was found
        panic!("Should have found at least one internal link to verify");
    }

    #[test]
    fn test_anchor_entities_created_in_full_export() {
        // Test that anchor entities are actually created during full export
        let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let kfx_data = build_kfx_container(&mut book, &|_, _, _, _| {}).unwrap();

        // Parse the KFX container to find anchor entities
        use crate::formats::kfx::container::{
            parse_container_header, parse_container_info, parse_index_table,
        };

        // 1. Parse header to get container_info location
        let header = parse_container_header(&kfx_data).expect("Failed to parse header");

        // 2. Parse container_info to get index table location
        let ci_start = header.container_info_offset;
        let ci_end = ci_start + header.container_info_length;
        let container_info = parse_container_info(&kfx_data[ci_start..ci_end])
            .expect("Failed to parse container info");

        // 3. Parse the index table
        let (idx_offset, idx_len) = container_info.index.expect("No index table");
        let index = parse_index_table(
            &kfx_data[idx_offset..idx_offset + idx_len],
            header.header_len,
        );

        // Find anchor entities (type 266 = $266 = Anchor)
        let anchor_count = index.iter().filter(|e| e.type_id == 266).count();

        // 人間失格 has no endnotes; its internal links are the 7 TOC targets
        // (はしがき, 第一〜第三の手記, 一, 二, あとがき) → 7 anchor entities.
        assert!(
            anchor_count >= 7,
            "Expected anchor entities for the TOC targets, got {}",
            anchor_count
        );
    }
}

#[cfg(test)]
mod manga_fxl_tests {
    use super::*;
    use crate::formats::kfx::fragment::FragmentData;

    /// Pages carrying no declared side, as a source without `page-spread-*`
    /// gives them.
    fn undeclared(n: usize) -> Vec<(usize, Option<crate::model::PageSpread>)> {
        (0..n).map(|i| (i, None)).collect()
    }

    /// The cover is a solo unit; the rest pair into consecutive spreads with an
    /// odd tail page standing alone. Every page index appears exactly once, in
    /// order — the facing order the device reads.
    #[test]
    fn page_groups_cover_solo_then_pairs_with_odd_tail() {
        let g = |n: usize| manga_page_groups(&undeclared(n), true);
        assert!(g(0).is_empty());
        assert_eq!(g(1), vec![vec![0]]);
        assert_eq!(g(2), vec![vec![0], vec![1]]);
        assert_eq!(g(3), vec![vec![0], vec![1, 2]]);
        assert_eq!(g(4), vec![vec![0], vec![1, 2], vec![3]]);
        assert_eq!(g(5), vec![vec![0], vec![1, 2], vec![3, 4]]);
        let flat: Vec<usize> = g(11).into_iter().flatten().collect();
        assert_eq!(flat, (0..11).collect::<Vec<_>>());
    }

    /// A source that states each page's side pairs on that statement, not on
    /// position: a left page opens a facing pair and the right page closes it,
    /// and a page whose partner is missing stands alone.
    #[test]
    fn page_groups_follow_the_declared_spread_sides() {
        use crate::model::PageSpread::{Left, Right};
        let pages = [
            (0, Some(Right)),
            (1, Some(Left)),
            (2, Some(Right)),
            (3, Some(Left)),
            (4, Some(Left)),
            (5, Some(Right)),
        ];
        assert_eq!(
            manga_page_groups(&pages, true),
            vec![vec![0], vec![1, 2], vec![3], vec![4, 5]]
        );
    }

    /// A landscape canvas carries one whole view per page; nothing pairs.
    #[test]
    fn a_landscape_canvas_pairs_nothing() {
        assert_eq!(
            manga_page_groups(&undeclared(4), false),
            vec![vec![0], vec![1], vec![2], vec![3]]
        );
    }

    /// A page image twice as wide, against the page box, as a single page is one
    /// facing spread shipped whole; a page matching the box is not.
    #[test]
    fn a_double_wide_page_reads_as_a_facing_spread() {
        assert!(is_facing_spread_image(3600, 2700, 1800, 2700, None));
        assert!(!is_facing_spread_image(1800, 2700, 1800, 2700, None));
        assert!(!is_facing_spread_image(1146, 1719, 1800, 2700, None));
        // A viewport half the canvas height states the same fact.
        assert!(is_facing_spread_image(
            1800,
            1350,
            1800,
            2700,
            Some((1800, 1350))
        ));
    }

    /// Each `categorised_metadata` category name with its keys, in order.
    fn metadata_categories(frag: &KfxFragment) -> Vec<(String, Vec<String>)> {
        let crate::formats::kfx::fragment::FragmentData::Ion(IonValue::Struct(fields)) = &frag.data
        else {
            panic!("expected an Ion struct");
        };
        let Some((_, IonValue::List(categories))) = fields
            .iter()
            .find(|(id, _)| *id == KfxSymbol::CategorisedMetadata as u64)
        else {
            panic!("expected categorised_metadata");
        };
        categories
            .iter()
            .map(|cat| {
                let IonValue::Struct(cat_fields) = cat else {
                    panic!("expected a category struct");
                };
                let name = match cat_fields
                    .iter()
                    .find(|(id, _)| *id == KfxSymbol::Category as u64)
                {
                    Some((_, IonValue::String(s))) => s.clone(),
                    _ => panic!("expected a category name"),
                };
                let entries = match cat_fields
                    .iter()
                    .find(|(id, _)| *id == KfxSymbol::Metadata as u64)
                {
                    Some((_, IonValue::List(list))) => list
                        .iter()
                        .map(|entry| {
                            let IonValue::Struct(kv) = entry else {
                                panic!("expected a key/value struct");
                            };
                            match kv.iter().find(|(id, _)| *id == KfxSymbol::Key as u64) {
                                Some((_, IonValue::String(k))) => k.clone(),
                                _ => panic!("expected a key"),
                            }
                        })
                        .collect(),
                    _ => panic!("expected a metadata list"),
                };
                (name, entries)
            })
            .collect()
    }

    /// `OrientationLock::Landscape` emits `kindle_ebook_metadata` holding
    /// `book_orientation_lock` alone.
    #[test]
    fn fixed_layout_book_states_its_orientation_lock() {
        let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let mut meta = book.metadata().clone();
        meta.fixed_layout = true;
        meta.orientation_lock = Some(crate::model::OrientationLock::Landscape);
        book.set_metadata_override(meta);

        let mut ctx = ExportContext::new();
        ctx.fixed_layout_book = true;
        let container_id = generate_container_id("test");
        let categories =
            metadata_categories(&build_book_metadata_fragment(&book, &container_id, &ctx));

        let ebook = categories
            .iter()
            .find(|(name, _)| name == "kindle_ebook_metadata")
            .expect("a locked fixed-layout book carries an ebook category");
        assert_eq!(ebook.1, vec!["book_orientation_lock".to_string()]);
    }

    /// `fixed_layout_book` without `orientation_lock` emits three categories.
    #[test]
    fn fixed_layout_book_without_a_lock_has_no_ebook_category() {
        let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let mut meta = book.metadata().clone();
        meta.fixed_layout = true;
        book.set_metadata_override(meta);

        let mut ctx = ExportContext::new();
        ctx.fixed_layout_book = true;
        let container_id = generate_container_id("test");
        let categories =
            metadata_categories(&build_book_metadata_fragment(&book, &container_id, &ctx));

        let names: Vec<&str> = categories.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "kindle_audit_metadata",
                "kindle_capability_metadata",
                "kindle_title_metadata",
            ]
        );
    }

    /// `fixed_layout_capabilities` emits Ion ints, `yj_double_page_spread`
    /// only alongside a spread.
    #[test]
    fn fixed_layout_capabilities_state_the_comic_reader_keys() {
        use crate::formats::kfx::metadata::MetadataValue::Int;
        assert_eq!(
            fixed_layout_capabilities(true),
            vec![
                ("continuous_popup_progression", Int(0)),
                ("yj_double_page_spread", Int(1)),
                ("yj_fixed_layout", Int(1)),
            ]
        );
        assert_eq!(
            fixed_layout_capabilities(false),
            vec![
                ("continuous_popup_progression", Int(0)),
                ("yj_fixed_layout", Int(1)),
            ]
        );
    }

    fn unit(is_cover: bool, pages: &[(u64, u64, u64)]) -> MangaUnit {
        MangaUnit {
            section_name: "c0".into(),
            section_sym: 1,
            story_sym: 2,
            pt_id: 100,
            solo: is_cover,
            pages: pages
                .iter()
                .map(|&(o, i, im)| MangaPage {
                    res_name: "e0".into(),
                    res_sym: 3,
                    thumb_name: String::new(),
                    thumb_sym: 0,
                    enc: 0,
                    outer_id: o,
                    inner_id: i,
                    image_id: im,
                })
                .collect(),
        }
    }

    /// A section's reading-position span drives both position maps and matches
    /// the EIDs laid down: a solo unit contributes page_template + 1, a facing
    /// unit page_template + 3 per page.
    #[test]
    fn unit_eids_span_cover_vs_spread() {
        let cover = unit(true, &[(0, 0, 101)]);
        assert_eq!(manga_unit_eids(&cover), vec![100, 101]);

        let spread = unit(false, &[(101, 102, 103), (104, 105, 106)]);
        assert_eq!(
            manga_unit_eids(&spread),
            vec![100, 101, 102, 103, 104, 105, 106]
        );
    }

    /// Collect the feature keys of a content_features fragment.
    fn feature_keys(frag: &KfxFragment) -> Vec<String> {
        let FragmentData::Ion(IonValue::Struct(fields)) = &frag.data else {
            panic!("content_features is not an Ion struct");
        };
        let mut keys = Vec::new();
        for (k, v) in fields {
            if *k == KfxSymbol::Features as u64
                && let IonValue::List(feats) = v
            {
                for feat in feats {
                    if let IonValue::Struct(fs) = feat {
                        for (fk, fv) in fs {
                            if *fk == KfxSymbol::Key as u64
                                && let IonValue::String(s) = fv
                            {
                                keys.push(s.clone());
                            }
                        }
                    }
                }
            }
        }
        keys
    }

    /// `yj_non_pdf_fixed_layout` is always advertised; `yj_double_page_spread`
    /// only with a real spread and `yj_thumbnails_present` only with a page
    /// thumbnail.
    #[test]
    fn content_features_gate_spread_and_thumbnails() {
        let full = feature_keys(&build_manga_content_features_fragment(true, true));
        assert!(full.iter().any(|k| k == "yj_non_pdf_fixed_layout"));
        assert!(full.iter().any(|k| k == "yj_double_page_spread"));
        assert!(full.iter().any(|k| k == "yj_thumbnails_present"));

        let minimal = feature_keys(&build_manga_content_features_fragment(false, false));
        assert!(minimal.iter().any(|k| k == "yj_non_pdf_fixed_layout"));
        assert!(!minimal.iter().any(|k| k == "yj_double_page_spread"));
        assert!(!minimal.iter().any(|k| k == "yj_thumbnails_present"));
    }

    /// document_data carries the book-level page-turn signal: horizontal_tb text
    /// axis + rtl direction for a manga.
    #[test]
    fn document_data_is_horizontal_rtl() {
        let mut ctx = ExportContext::new();
        ctx.register_section("c0");
        ctx.document_writing_mode = KfxSymbol::HorizontalTb;
        ctx.document_direction = KfxSymbol::Rtl;
        let frag = build_manga_document_data_fragment(&ctx, Some(KfxSymbol::Rtl));
        let FragmentData::Ion(IonValue::Struct(fields)) = &frag.data else {
            panic!("document_data is not an Ion struct");
        };
        let get = |sym: KfxSymbol| {
            fields
                .iter()
                .find(|(k, _)| *k == sym as u64)
                .map(|(_, v)| v)
        };
        assert!(matches!(
            get(KfxSymbol::WritingMode),
            Some(IonValue::Symbol(s)) if *s == KfxSymbol::HorizontalTb as u64
        ));
        assert!(matches!(
            get(KfxSymbol::Direction),
            Some(IonValue::Symbol(s)) if *s == KfxSymbol::Rtl as u64
        ));
        assert!(matches!(
            get(KfxSymbol::SpacingPercentBase),
            Some(IonValue::Symbol(s)) if *s == KfxSymbol::Width as u64
        ));
    }
}
