//! KFX format exporter.
//!
//! This module provides the `KfxExporter` which implements the `Exporter` trait
//! for writing books in Amazon's KFX format.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, Seek, Write};

use crate::export::Exporter;
use crate::formats::kfx::auxiliary::{build_auxiliary_data_fragment, build_ruby_content_fragments};
use crate::formats::kfx::context::{ExportContext, LandmarkTarget};
use crate::formats::kfx::cover::{
    COVER_SECTION_NAME, build_cover_section, get_chapter_image_path, is_image_only_chapter,
    needs_standalone_cover, normalize_cover_path,
};
use crate::formats::kfx::fragment::KfxFragment;
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

/// Build a complete KFX container from a book.
///
/// This follows a strict Two-Pass architecture:
/// - Pass 1 (Survey): Walk IR, build position map, intern symbols - NO ION GENERATION
/// - Pass 2 (Synthesis): Generate Ion using pre-computed positions
fn build_kfx_container(
    book: &mut Book,
    on_progress: &dyn Fn(&str, usize, usize, &str),
) -> io::Result<Vec<u8>> {
    // A pre-paginated image book (manga/comic) is not reflowable content — emit
    // a fixed-layout `yj_non_pdf_fixed_layout` KFX instead of flowing every page
    // into one scrolling column. See [`image_fxl_to_kfx`].
    if is_fixed_layout_image_book(book) {
        return image_fxl_to_kfx(book, on_progress);
    }

    let container_id = generate_container_id(&book.metadata().title);
    let mut ctx = ExportContext::new();

    // ========================================================================
    // PASS 1: SURVEY (Read-Only / State Accumulation)
    // Goal: Populate ctx.symbols, ctx.position_map, ctx.chapter_fragments
    // NO ION GENERATION HERE!
    // ========================================================================

    // Check if we need a standalone cover section
    // This happens when the EPUB cover image differs from the first spine chapter's image
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
                        // For dimension probe we want the file that's actually going
                        // to render as the cover. Standalone path: the metadata cover.
                        // In-spine titlepage path: whatever single image that chapter
                        // hosts (which may or may not be the same file).
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
    // Probe the cover image's pixel dimensions once, in Pass 1, so both
    // emission paths (standalone c0 in `build_cover_section` and in-spine
    // image-only-chapter in `build_chapter_entities_grouped`) can size the
    // page_template's `fixed_width` / `fixed_height` to the actual image.
    // Amazon's encoder always matches them; mismatched dimensions plus
    // `scale_fit` produces the cover-with-margins bug.
    if let Some(ref p) = probe_path
        && let Ok(bytes) = book.load_asset(std::path::Path::new(p))
        && let Some(dims) = crate::util::extract_image_dimensions(&bytes)
    {
        ctx.cover_dimensions = Some(dims);
    }

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
        // Assign fragment ID for cover section now (used by landmarks)
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

    // 1a. Resolve all links using the centralized resolver
    // This builds the forward/reverse link maps and resolves TOC targets.
    let resolved = book.resolve_links()?;

    // 1b. Register link targets with the anchor registry
    // This maps hrefs to targets for storyline link_to generation.
    register_link_targets(book, &spine_info, &resolved, &mut ctx)?;

    // 1c. Survey each chapter: assign fragment IDs, build position map
    // Also build a map from source paths to chapter IDs for landmark resolution
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

    // 1d. Resolve landmarks to fragment IDs
    // First try IR landmarks, then fall back to heuristics for Cover/StartReading
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

                // Stop once we have both
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

    // After Pass 1: ctx.symbols is COMPLETE, ctx.position_map has all EIDs
    // Note: TOC anchor entity IDs are computed AFTER Pass 2 chapter processing
    // since anchors are created during content generation.

    // Determine the document-level writing-mode by scanning IR style pools
    // across every chapter. This must happen *before* Pass 2 starts
    // registering KFX styles — otherwise `extract_ir_field` can't decide
    // whether an explicit `horizontal-tb` cascade result is an override
    // (in a vertical book) or just the spec default, and the horizontal
    // override silently disappears into KFX's `vertical_rl` inheritance.
    ctx.document_writing_mode = book_writing_mode(book);
    ctx.document_direction = document_direction(book, ctx.document_writing_mode);
    // Per-style `writing_mode` overrides compare against the SOURCE's own
    // content-derived mode, not the (possibly user-forced) document mode above.
    // Otherwise a horizontal book force-set to vertical stamps `horizontal_tb` on
    // every style and cancels the vertical document default — the device rotates
    // each glyph, the webview falls back to horizontal. Unforced, these coincide.
    ctx.style_writing_mode_baseline = dominant_writing_mode_from_ir(book);
    // The body font defers to the reader's choice — see `reader_font_family`.
    // Without this the content pins the device font and the Kindle's font
    // control has nothing left to override.
    ctx.reader_font_family = dominant_generic_font_family(book);
    // Stamp a device-recognized content language on every reflowable style so the
    // Kindle selects CJK fonts + upright vertical orientation. Without it a
    // Chinese/Japanese book renders in Latin fonts with sideways glyphs — Amazon
    // stamps this on its own styles (e.g. `zh-tw`); bokai previously did not.
    ctx.content_language =
        crate::formats::kfx::metadata::kfx_content_language(&book.metadata().language);

    // ========================================================================
    // PASS 2: SYNTHESIS (Generate Ion)
    // Now ctx.position_map is populated. We can resolve links correctly.
    // ========================================================================

    let mut fragments = Vec::new();

    // Entity order matches reference KFX:
    // 1. content_features ($585)
    // 2. book_metadata ($490)
    // 3. metadata ($258)
    // 4. document_data ($538)
    // 5. book_navigation ($389)
    // 6+. sections ($260) - all together
    // N+. storylines ($259) - all together
    // M+. content ($145) - all together

    // 2a. Content features fragment ($585)
    fragments.push(build_content_features_fragment(&ctx));

    // 2b. Book metadata fragment ($490) - contains categorised_metadata
    fragments.push(build_book_metadata_fragment(book, &container_id, &ctx));

    // 2c. Metadata fragment ($258) - contains reading_orders
    fragments.push(build_metadata_fragment(book.metadata(), &ctx));

    // NOTE: document_data ($538) is built AFTER chapters so max_id includes all content IDs.
    // We'll insert it at this position (index 3) later.
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
        // Store cover content ID for position_map (so c0 contains both section and content IDs)
        ctx.cover_content_id = Some(cover_content_id);
        // Probe the cover image for its actual pixel dimensions. Amazon's KFX
        // encoder sizes the cover page_template's fixed_width / fixed_height
        // to the image's resource dimensions; any mismatch causes the device
        // to letterbox/pillarbox with `scale_fit`. Falls back to a sane
        // book-cover aspect default when probing fails.
        let (section, storyline) = build_cover_section(cover_path, section_id, &mut ctx);
        section_fragments.push(section);
        storyline_fragments.push(storyline);

        // Point the cover landmark at the section's page-template id (== section_id,
        // the container position), NOT the content/storyline id. A real Amazon KFX's
        // `cover_page` landmark targets the cover section's page_template `id` — that
        // is what makes the device render the cover full-screen (no chrome, black
        // letterbox) instead of as an ordinary flowed page. This overrides whatever
        // the IR landmark resolution set. `cover_content_id` is kept for the position map.
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

            // Record which image resources this section depends on, so the
            // container_entity_map can declare the dependency graph that
            // Kindle uses to locate images.
            for node_id in chapter.iter_dfs() {
                if let Some(node) = chapter.node(node_id)
                    && node.role == crate::model::Role::Image
                    && let Some(src) = chapter.semantics.src(node_id)
                {
                    let short_name = ctx.resource_registry.get_or_create_name(src);
                    ctx.record_section_image_ref(section_name, &short_name);
                }
            }
        }
    }

    // Fix landmark IDs to use storyline content IDs instead of section IDs
    ctx.fix_landmark_content_ids();

    // 2e. Book navigation fragment - built AFTER chapters so heading/anchor positions are available
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

    // 2j. Resource fragments (images, fonts, etc.)
    // Each resource gets two entities: external_resource (metadata) + bcRawMedia (bytes).
    //
    // Interior images run through `encode_asset_for_kfx`: rasters (and
    // rasterized SVGs — KFX has no vector resource format) become grayscale
    // JXR plates; anything undecodable falls back to the JPEG sanitize path.
    // The cover image stays on the JPEG path (`sanitize_for_kfx`: JFIF
    // re-encode / APP-segment strip): the Kindle library-gallery and
    // sleep-screen thumbnailer don't read a JXR cover (the book opens fine,
    // but the thumbnail/screensaver go blank), and a KOA2's stricter
    // screensaver JPEG decoder rejects EXIF-tagged covers.
    let cover_filename = book.metadata().cover_image.as_ref().and_then(|c| {
        std::path::Path::new(c)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
    });
    // Interior plates become JXR in the book's chosen color mode (default
    // grayscale). Captured before the loop so the `book` borrow inside is free.
    let color_mode = book.image_color_mode();
    // Bundle only resources something can actually load: images referenced by
    // a section (`section_resource_deps` is recorded from the IR during the
    // chapter loop above, and `build_cover_section` records the standalone
    // cover), the metadata cover, and fonts (matched to @font-face rules in
    // `build_font_fragments` below). EPUBs routinely ship images nothing
    // displays — e.g. `display:none` raster fallbacks next to each SVG
    // figure, which the IR transform already drops — and bundling them
    // bloats the container with resources no storyline can reach.
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
            let is_cover =
                cover_filename.as_deref() == asset_path.file_name().and_then(|s| s.to_str());
            let bundled = if is_cover {
                crate::image::jpeg::sanitize_for_kfx(&data).unwrap_or(data)
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

    // 2k. Navigation maps. Section-keyed position architecture
    // (`position_id_map` + one `section_position_id_map` per section), matching
    // Amazon's reflowable KFX — this is what makes a section-level nav target
    // (the cover) resolvable on-device. `section_names` is the per-section name
    // string in `section_ids` order (cover first when standalone), used to key
    // the `section_position_id_map` entities.
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

    // 2l. Container metadata entities
    fragments.push(build_resource_path_fragment());
    fragments.push(build_container_entity_map_fragment(
        &container_id,
        &fragments,
        &ctx,
    ));

    // 2d. Document data fragment ($538) - built AFTER all IDs are assigned so max_id is correct
    // Insert at position 3 (after content_features, book_metadata, metadata)
    fragments.insert(document_data_index, build_document_data_fragment(&ctx));

    // Build symbol table ION using context
    let local_syms = ctx.symbols.local_symbols();
    let symtab_ion = build_symbol_table_ion(local_syms);

    // Build format capabilities ION
    let format_caps_ion = build_format_capabilities_ion();

    // Serialize fragments to entities
    let entities = serialize_fragments(&fragments, ctx.symbols.local_symbols());

    // ========================================================================
    // PASS 3: SERIALIZATION
    // ========================================================================

    on_progress("finalize", 1, 1, "Finalizing");
    Ok(serialize_container(
        &container_id,
        &entities,
        &symtab_ion,
        &format_caps_ion,
    ))
}

// ============================================================================
// Pass 1: Survey Functions (NO ION GENERATION)
// ============================================================================

/// Survey a chapter during Pass 1.
///
/// This walks the IR tree to:
/// - Assign a fragment ID to this chapter
/// - Build position map entries for every node
/// - Intern all text and attribute strings
/// - Track text offsets for link resolution
///
/// NO ION GENERATION happens here.
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

    // Note: Heading positions are recorded during Pass 2 in tokens_to_ion()
    // where actual content fragment IDs are available.
    // Anchor entities are created during Pass 2 using GlobalNodeId targets
    // from ResolvedLinks.

    // Register resources (src attributes) - creates short names like "e0"
    // Note: href and alt are used as string values, not symbols
    if let Some(src) = chapter.semantics.src(node_id) {
        ctx.resource_registry.register(src, &mut ctx.symbols);
    }

    // Track text content and advance offset
    if !node.text.is_empty() {
        let text = chapter.text(node.text);
        ctx.advance_text_offset(text.len());
        // We don't need to intern plain text content
    }

    // Recurse into children
    for child in chapter.children(node_id) {
        survey_node(chapter, child, ctx);
    }
}

/// Register link targets from ResolvedLinks with the AnchorRegistry.
///
/// This walks all chapters and registers each link's target with the
/// anchor registry, mapping hrefs to their resolved targets (GlobalNodeId,
/// ChapterId, or external URL).
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
                    // Body-level ids (promoted to NodeId::ROOT by html::transform)
                    // have no element to anchor to. Register as a chapter-level
                    // target instead — the chapter's first content fragment IS
                    // where a body-id link should land. Without this, the link
                    // generates an `a<N>` symbol but no Anchor entity, leaving
                    // an orphan link_to on Kindle.
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

/// Build style fragments from the registry.
///
/// KFX requires every storyline element to have a style reference.
/// This generates all collected styles from the registry, including the default.
fn build_style_fragments(ctx: &mut ExportContext) -> Vec<KfxFragment> {
    // `ctx.document_writing_mode` was set before Pass 2 by
    // `dominant_writing_mode_from_ir` (scanning IR style pools), so the
    // ingest pipeline could compare each style's `writing_mode` against it
    // when deciding what to emit.

    // Normalise per-paragraph line-height values to `lh` ratios so Kindle's
    // Spacing slider can scale them. The body's dominant line-height
    // becomes `1.0 lh`; outliers (tighter notes, looser headings) carry
    // proportional ratios. Document_data baseline stays at 1.2 em, so the
    // rendered body line-height is 1.0 × 1.2em = 1.2em at slider default —
    // tighter than the source CSS asks for, matching the publisher KFX's
    // E-Ink-optimised default.
    ctx.style_registry.normalize_line_heights_to_lh();

    // Drain all styles from the registry to generate Ion fragments, stamping the
    // book's content language on each (so the device selects CJK fonts + upright
    // vertical orientation). Cloned first to avoid borrowing `ctx` twice.
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

/// Build the book metadata fragment ($490) - contains categorised_metadata.
///
/// Uses the metadata schema to map IR metadata to KFX categories.
/// To add new metadata fields, update the schema in `kfx/metadata.rs`.
fn build_book_metadata_fragment(
    book: &Book,
    container_id: &str,
    ctx: &ExportContext,
) -> KfxFragment {
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
        // Search for a resource ending with this filename. Pick the
        // lexicographically smallest match so a basename shared across
        // directories resolves deterministically, not in HashMap order.
        ctx.resource_registry
            .iter()
            .map(|(href, _)| href)
            .filter(|href| href.ends_with(filename))
            .min()
            .and_then(|href| ctx.resource_registry.get_name(href))
    });

    // book_id: reuse `meta.identifier` if it already has the KFX shape (23-char
    // URL-safe Base64). Otherwise derive deterministically. Reuse keeps the
    // identifier stable across a KFX → EPUB → KFX round trip.
    let book_id = if !meta.identifier.is_empty() {
        if looks_like_kfx_book_id(&meta.identifier) {
            Some(meta.identifier.clone())
        } else {
            Some(generate_book_id(&meta.identifier))
        }
    } else {
        None
    };

    // ASIN: pass through when the source carries a real Amazon catalogue
    // value; otherwise synthesize from the identifier so PDOC sideloads
    // get a stable library-tile cover-cache key. Logic lives in
    // `kfx::metadata::resolve_export_asin` so a caller can call the same
    // function to learn what we stamp here (needed to manage the on-device
    // `<title>_<ASIN>.sdr/` sidecar Kindle invents).
    let asin = crate::formats::kfx::metadata::resolve_export_asin(meta);

    // content_id mirrors ASIN (calibre convention). The device `.sdr`
    // directory uses this as the per-book state key; matching ASIN means
    // kfx-zip → kfx round-trips preserve the binding the user already has
    // on the device. Schema keeps them as separate `MetadataContext` slots
    // so future divergence is possible, but at write time they're equal.
    let content_id = asin.clone();

    let meta_ctx = MetadataContext {
        version: Some(env!("CARGO_PKG_VERSION")),
        cover_resource_name,
        asset_id: Some(container_id),
        book_id,
        asin,
        content_id,
    };

    // Build each category using the schema. Order matches calibre's KFX:
    // ebook → title → audit → capability (empty list, but its presence
    // appears to be what makes the device library service treat the file as
    // a complete Kindle book.)
    let categories = [
        MetadataCategory::KindleEbook,
        MetadataCategory::KindleTitle,
        MetadataCategory::KindleAudit,
        MetadataCategory::KindleCapability,
    ];

    let categorised: Vec<IonValue> = categories
        .iter()
        .map(|&cat| {
            let entries = build_category_entries(cat, meta, &meta_ctx);
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

/// Helper to create a metadata key-value struct. `value` may be a string or
/// an Ion-native boolean (Amazon and calibre both emit `is_sample` and
/// `override_kindle_font` as bool literals).
fn metadata_kv(key: &str, value: &crate::formats::kfx::metadata::MetadataValue) -> IonValue {
    let ion_value = match value {
        crate::formats::kfx::metadata::MetadataValue::Text(s) => IonValue::String(s.clone()),
        crate::formats::kfx::metadata::MetadataValue::Bool(b) => IonValue::Bool(*b),
    };
    IonValue::Struct(vec![
        (KfxSymbol::Key as u64, IonValue::String(key.to_string())),
        (KfxSymbol::Value as u64, ion_value),
    ])
}

// KFX book_id shape: 23 chars, URL-safe Base64 alphabet. Matches what
// `generate_book_id` emits, so reusing a passthrough value is safe.
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

/// Build the content features fragment ($585).
///
/// This describes the content capabilities/features of the book.
fn build_content_features_fragment(ctx: &ExportContext) -> KfxFragment {
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

    let yj_hdv = IonValue::Struct(vec![
        (
            KfxSymbol::Namespace as u64,
            IonValue::String("com.amazon.yjconversion".to_string()),
        ),
        (
            KfxSymbol::Key as u64,
            IonValue::String("yj_hdv".to_string()),
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

    let mut features = vec![reflow_style, canonical_format, yj_hdv];

    // CJK reflow-language marker. Amazon stamps this alongside the book
    // `language` to switch the device into its per-script reflow typography
    // (CJK fonts, upright short numbers, vertical spacing). Without it a
    // Chinese/Japanese book renders with Latin fonts and rotated glyphs even
    // when the `language` tag is right. `content_language` is the per-style
    // form ("zh-tw"/"ja"/...), which the classifier maps back to the marker.
    if let Some((key, major)) =
        crate::formats::kfx::metadata::cjk_reflow_feature(&ctx.content_language)
    {
        features.push(content_feature("com.amazon.yjconversion", key, major));

        // Japanese vertical layout has a dedicated feature (no Chinese analog
        // exists in Amazon's table — Chinese vertical rides on the base marker
        // plus `document_data.writing_mode`).
        let vertical = matches!(
            ctx.document_writing_mode,
            KfxSymbol::VerticalRl | KfxSymbol::VerticalLr
        );
        if vertical && key == "jp-reflow-language" {
            features.push(content_feature(
                "com.amazon.yjconversion",
                "jpvertical-reflow-language",
                6,
            ));
        }
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

    // Picked up earlier by `build_style_fragments` (before the registry was
    // drained). KOA2 reads this to decide whether to expose the vertical-text
    // layout controls (Alignment greyed out, vertical Margins/Spacing icons).
    // Without it, vertical books render vertically *but* the device thinks
    // they're horizontal and shows the wrong UI affordances.
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
        // NOTE: `spacing_percent_base: width` was emitted here historically
        // but pins percentage-spacing to the horizontal axis. In vertical-rl
        // books that locks the device's Layout > Spacing slider to the wrong
        // axis — it ends up adjusting left/right page margins instead of the
        // column-to-column line spacing. Calibre-generated KFX omits this
        // field entirely; we follow suit and let the device default rule.
        (
            KfxSymbol::ReadingOrders as u64,
            IonValue::List(vec![reading_order]),
        ),
    ]);

    KfxFragment::singleton(KfxSymbol::DocumentData, document_data)
}

/// Resolve `document_data.direction` from the book's page-progression.
///
/// The device computes page-progression as `direction`, then overrides it to
/// rtl when `writing_mode` ends in `-rl`. So a vertical-rl book reaches rtl via
/// the writing-mode override and keeps `direction: ltr` (matching Amazon). A
/// book that turns right-to-left but whose writing mode is *not* `vertical_rl`
/// — a horizontal rtl manga, a horizontally-typeset rtl title — has no writing
/// mode to carry the turn, so `direction` itself must be `rtl`. Everything else
/// is `ltr` (the device default).
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

/// The book-level writing mode to stamp into `document_data.writing_mode`.
///
/// Writing mode is a fixed, book-level property: the publisher declares it once
/// and individual pages override it as the exception. The authoritative source
/// is the OPF `<meta name="primary-writing-mode">` hint, so prefer it whenever
/// present. Only when the book omits it (many publisher EPUBs, including the
/// LV999 series, express the mode purely through CSS on content roots) is there
/// nothing fixed to read, and the mode must be recovered from the content — see
/// [`dominant_writing_mode_from_ir`].
fn book_writing_mode(book: &mut Book) -> KfxSymbol {
    // `primary-writing-mode` is Amazon's book-level hint, and its vocabulary
    // (`horizontal-lr`, `horizontal-rl`, `vertical-rl`, `vertical-lr`) encodes
    // both the text axis *and* the page-turn direction — it is NOT a CSS
    // `writing-mode`. Only the axis maps to KFX `writing_mode`, so both
    // `horizontal-*` values collapse to `horizontal_tb`. (The page-turn
    // direction rides on the OPF spine `page-progression-direction`, applied
    // separately.)
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

/// Recover the book-level writing mode from the content when the OPF declares
/// no `primary-writing-mode` (see [`book_writing_mode`]), by scanning every
/// chapter's IR style pool.
///
/// This runs *before* Pass 2 so the answer is available while IR styles are
/// being ingested. The ingest side then compares each style's `writing_mode`
/// against this — emitting `writing_mode: horizontal_tb` on overrides in
/// vertical books, which the previous KFX-side scan couldn't recover
/// because `extract_ir_field` had already filtered horizontal-tb out as
/// the CSS-spec default.
///
/// Distinct styles in the pool are counted. Only the vertical axes are
/// tallied; [`pick_document_writing_mode`] then decides the book-level mode.
/// Is this font family a *category* rather than a typeface?
///
/// CSS generics (`serif`, `sans-serif`, …) name a category and let the renderer
/// pick the face. Kindle extends that with locale-specific generics —
/// `serif-ja`, `sans-serif-ja`, and their `-v` vertical variants — which are
/// still categories, not typefaces. `標楷體` or `MS Mincho` are typefaces.
///
/// The distinction matters because KFX resolves the two differently: a named
/// family pins the device font, while `default` defers to the reader's own
/// choice. Deferring is the faithful reading of a category.
fn is_generic_font_family(name: &str) -> bool {
    const GENERICS: &[&str] = &[
        "serif",
        "sans-serif",
        "monospace",
        "cursive",
        "fantasy",
        "system-ui",
    ];
    let n = name.trim().trim_matches('"').to_ascii_lowercase();
    // `serif-ja-v` is the vertical cut of the `serif-ja` category.
    let n = n.strip_suffix("-v").unwrap_or(&n);
    GENERICS
        .iter()
        .any(|g| n == *g || n.strip_prefix(g).is_some_and(|rest| rest.starts_with('-')))
}

/// The font family the book uses for most of its text, when that family is a
/// generic category — the one a reader's font setting is meant to replace.
///
/// **Why this exists.** A source stylesheet says `font-family: serif-ja, serif`
/// on its body text, meaning "set this in a serif face". Carried through
/// literally, every KFX style names `serif-ja`, the content pins the device
/// font, and the Kindle's font control stops doing anything — the setting has
/// nothing left to override. Amazon's own converter emits `default` for the
/// bulk of a book's styles for exactly this reason, keeping a specific family
/// only where the publisher asked for a *contrast*.
///
/// So only the dominant generic defers. A heading set in `sans-serif-ja`
/// against `serif-ja` body text is a deliberate distinction and survives; a
/// typeface the publisher actually named (`標楷體`) is never touched, since
/// naming one is a choice a category-level rule has no business overriding.
fn dominant_generic_font_family(book: &mut Book) -> Option<String> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let chapter_ids: Vec<_> = book.spine().iter().map(|e| e.id).collect();
    for chapter_id in chapter_ids {
        let Ok(chapter) = book.load_chapter(chapter_id) else {
            continue;
        };
        for (_, style) in chapter.styles.iter() {
            if let Some(f) = &style.font_family {
                *counts.entry(f.clone()).or_default() += 1;
            }
        }
    }
    // Ties break by name so the choice is reproducible across runs rather than
    // riding HashMap order.
    let (family, _) = counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))?;
    is_generic_font_family(&family).then_some(family)
}

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

/// Choose the book-level writing mode from the count of vertical styles.
///
/// `horizontal_tb` is the CSS initial value, so it is the writing mode of every
/// style that never declares one — including the scaffolding of fixed-layout
/// image pages. In a book that mixes vertical-rl body text with many such pages
/// (an illustrated light novel like the LV999 series), that default floods the
/// tally, and a naive "vertical must outnumber horizontal" majority wrongly
/// settles on horizontal_tb. The device then derives page-progression from
/// document_data (`direction` ltr + a writing mode whose suffix is not `-rl`)
/// and turns pages left-to-right — wrong for a Japanese book. Amazon's own KFX
/// sets the document writing mode to the book's primary *text* flow and marks
/// horizontal runs as the exception, so any genuine vertical text makes the
/// book vertical. Mirror that (and `validate::fidelity::writing_mode::dominant_mode`):
/// treat any vertical writing mode as decisive, picking the more-cited axis,
/// and fall back to horizontal_tb only when the book declares no vertical text
/// at all.
fn pick_document_writing_mode(vrl: usize, vlr: usize) -> KfxSymbol {
    if vrl == 0 && vlr == 0 {
        KfxSymbol::HorizontalTb
    } else if vrl >= vlr {
        KfxSymbol::VerticalRl
    } else {
        KfxSymbol::VerticalLr
    }
}

/// Build the book navigation fragment with resolved positions.
///
/// Uses ctx.position_map to generate correct fid:off positions for TOC entries.
/// Structure: [{reading_order_name: default, nav_containers: [nav_container::{...}, ...]}]
/// Order matches reference KFX: headings, toc, landmarks
fn build_book_navigation_fragment_with_positions(book: &Book, ctx: &ExportContext) -> KfxFragment {
    let mut nav_containers = Vec::new();

    // 0. Add page_list nav container FIRST (Amazon's canonical order): physical
    //    page numbers → content positions, driving "go to page N" and citation
    //    location↔page mapping. Flat, one nav_unit per printed page. Empty when
    //    the source EPUB carries no `<nav epub:type="page-list">`.
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

    // 2. Add TOC nav container. Amazon's KFX lists the cover (表紙 / "Cover
    //    Page") as the FIRST toc entry even though source EPUB TOCs never do —
    //    it's what makes the cover both appear in the device's "Go To ›
    //    Contents" and JUMP there. Prepend a synthesized one (see
    //    `build_cover_toc_entry`), pointing at the cover's first *content*
    //    position (NOT the page-template section root, which isn't in the
    //    position map and wedges the firmware).
    // Drop any cover entry the source TOC carries itself (合本 editions / round
    // -tripped Amazon books list 表紙 at a content eid), then prepend the
    // canonical cover → section root, so the cover appears exactly once and
    // shares the `cover_page` landmark's id (which is what makes it jump
    // on-device — the firmware merges same-id TOC + landmark).
    let cover_eids = cover_section_eids(ctx);
    let cov_label = cover_label(book);
    let src_toc: Vec<crate::model::TocEntry> = book
        .toc()
        .iter()
        .filter(|e| {
            // By title: a 表紙 / Cover entry is the source's own cover — catches a
            // round-tripped entry whose href mis-resolves outside the cover
            // section. (Real publisher EPUBs never list the cover in the TOC.)
            let title = e.title.trim();
            if title == cov_label
                || title.eq_ignore_ascii_case("cover")
                || title.eq_ignore_ascii_case("cover page")
            {
                return false;
            }
            // By position: an entry landing in the cover section is the cover.
            match resolve_toc_target(&e.target, &e.href, ctx) {
                Some((fid, _)) => !cover_eids.contains(&fid),
                None => true,
            }
        })
        .cloned()
        .collect();
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

/// Build headings navigation entries grouped by heading level.
///
/// Structure: Each heading level (h2, h3, etc.) gets a nav_unit with nested
/// entries for all headings of that level.
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

/// Build landmarks navigation entries.
///
/// Build landmark entries from resolved landmarks using schema mapping.
///
/// Iterates over all landmarks in ctx.landmark_fragments and converts each
/// to a KFX nav_unit using the schema for type conversion.
fn build_landmarks_entries(book: &Book, ctx: &ExportContext) -> Vec<IonValue> {
    use crate::formats::kfx::schema::schema;

    let mut entries = Vec::new();

    // Sort landmarks for consistent output (Cover first, then StartReading, then
    // others). `landmark_fragments` is a HashMap, so the collected order is
    // per-process random; the sort key must be TOTAL or same-rank landmarks
    // (all the "others") leak that random order into the bytes. Tie-break by
    // reading position, then landmark type (the unique map key) so the order is
    // fully deterministic.
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

        // The cover TOC entry and this `cover_page` landmark share one section
        // id; the device merges them into a single Go-To item shown with the
        // LANDMARK's label. So give the cover the localized cover word (表紙) —
        // matching the TOC entry and the desktop reader — instead of the source
        // EPUB's label ("Cover").
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

/// Build the cover's leading TOC nav_unit — the entry Amazon's KFX always puts
/// first in its `toc` nav container (表紙 / "Cover Page").
///
/// Targets the cover **section root** — the SAME id the `cover_page` landmark
/// uses. With the section-keyed position architecture (one
/// `section_position_id_map` per section; see [`section_positions`]) the section
/// root is a navigable position, so the device resolves this target and merges
/// it with the landmark into a single jumpable 表紙 — exactly Amazon's shape,
/// whose cover TOC entry and `cover_page` landmark share one section id. (A
/// content-eid target instead gets dropped: the device keeps the landmark and
/// drops a *separate* cover entry that lands in the same section.) Returns `None`
/// when there's no cover landmark, or when the source TOC already leads with the
/// cover.
/// The cover's display word, localized like Amazon (表紙 for Japanese books,
/// else "Cover"). Used for the cover TOC entry, the `cover_page` landmark (the
/// device shows the landmark's label for the merged Go-To item), and to detect
/// the source TOC's own cover entry.
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

/// EIDs of the cover section (root + content). Used to strip a cover entry the
/// source TOC carries itself — some EPUBs (合本 editions, KFX→EPUB round-trips)
/// list 表紙 pointing at a content eid — so the canonical cover entry (prepended
/// at the section root, sharing the landmark's id) is the only one. Empty when
/// the book has no cover.
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

/// Build TOC entries recursively with anchor entity IDs.
///
/// TOC entries point to content fragment IDs (with offset 0) rather than
/// anchor entities. The `entry.target` field is pre-resolved by `resolve_links()`.
fn build_toc_entries_with_positions(
    entries: &[crate::model::TocEntry],
    ctx: &ExportContext,
) -> Vec<IonValue> {
    entries
        .iter()
        .filter_map(|entry| {
            // Use pre-resolved target to look up position
            let (fragment_id, offset) = resolve_toc_target(&entry.target, &entry.href, ctx)?;

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

            // Add children if present
            if !entry.children.is_empty() {
                let child_entries = build_toc_entries_with_positions(&entry.children, ctx);
                if !child_entries.is_empty() {
                    fields.push((KfxSymbol::Entries as u64, IonValue::List(child_entries)));
                }
            }

            let nav_unit = IonValue::Struct(fields);
            // Annotate with nav_unit::
            Some(IonValue::Annotated(
                vec![KfxSymbol::NavUnit as u64],
                Box::new(nav_unit),
            ))
        })
        .collect()
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
/// mapping the printed page label to its content position. Matches Amazon's
/// shape: `nav_unit::{representation:{label}, target_position:{id, offset}}`.
/// Entries whose href doesn't resolve are dropped (a dangling nav target makes
/// the device reject the whole book).
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

/// Resolve a page-list entry's target to `(fragment_id, offset)`, **preserving
/// the byte offset** — the one difference from [`resolve_toc_target`], which
/// forces offset 0 to land TOC jumps on chapter starts. A physical page break
/// routinely falls mid-chapter (`…#page_N`), and its precise offset is what
/// makes "go to page N" land on the right line. Falls back to the chapter start
/// (offset 0) only for a body-level id or a fragment-less chapter href.
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

// ============================================================================
// Entity Assembler: Packages Schema output into KFX Entity Hierarchy
// ============================================================================

/// Build chapter entities returning them separately for grouped emission.
///
/// Returns (section, storyline, Option<content>) so they can be grouped by type.
fn build_chapter_entities_grouped(
    chapter: &Chapter,
    chapter_id: ChapterId,
    section_name: &str,
    ctx: &mut ExportContext,
) -> (KfxFragment, KfxFragment, Option<KfxFragment>) {
    use crate::formats::kfx::storyline::{ir_to_tokens, tokens_to_ion};

    // Check if this is a cover chapter (image-only).
    // Three gates:
    //   1. No standalone c0 — when `cover_fragment_id` is set, c0 already
    //      handles the cover so no in-spine chapter should claim it.
    //   2. Chapter is image-only (one Image node, no text).
    //   3. No in-spine cover has been claimed yet. EPUBs that put SVG-wrapped
    //      thumbnails on every section landing page (e.g. `<div id="toc-N">
    //      <svg><image/></svg></div>`) look image-only too; without this
    //      gate they'd all be treated as covers and lose their wrapping
    //      `<div id>` anchors when re-emitted via build_cover_storyline.
    let is_cover = ctx.cover_fragment_id.is_none()
        && !ctx.inline_cover_emitted
        && is_image_only_chapter(chapter);

    // =========================================================================
    // 1. SETUP: Naming for this chapter's entity triad
    // =========================================================================
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

    // For an in-spine cover, repoint the `cover_page` landmark at this section's
    // page-template id (the container position == section_id). The IR landmark
    // resolver defaults it to the storyline id; a real Amazon KFX targets the
    // page-template id, which is what makes the device render the cover
    // full-screen (no chrome) instead of as an ordinary flowed page.
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
    } else {
        // Normal chapter: full token-based generation
        let tokens = ir_to_tokens(chapter, ctx);
        let content_list = tokens_to_ion(&tokens, ctx);
        let text = ctx.drain_text();
        (content_list, text)
    };

    // =========================================================================
    // 3. ASSEMBLE: Package into three KFX Entities
    // =========================================================================

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
        // Cover page: container type sized to the cover image's actual pixel
        // dimensions. Matching the resource exactly is what Amazon's encoder
        // does — `scale_fit` with mismatched dims letterbox/pillarboxes (the
        // cover-with-margins bug). Falls back to a generic book-cover aspect
        // when the Pass-1 probe failed.
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
        IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Int(section_id as i64)),
            (
                KfxSymbol::StoryName as u64,
                IonValue::Symbol(story_name_symbol),
            ),
            (
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::Text as u64),
            ),
        ])
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

/// Build a simplified storyline for cover chapters.
///
/// Cover pages have a flat structure with just the image directly in content_list,
/// no container wrapper. Structure: [{ type: image, resource_name, style }]
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

                // Resolve pending chapter-start anchor. The cover path doesn't
                // go through StartElement → resolve_pending in storyline.rs,
                // so chapters that are body-id link targets but also covers
                // (e.g. `<body epub:type="cover" id="...">`) would otherwise
                // leave an orphan link_to.
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
///
/// This is the "Assembler" (Macro layer) that:
/// 1. Sets up naming for this chapter's entity triad
/// 2. Calls schema-driven token generation (`ir_to_tokens`)
/// 3. Calls `tokens_to_ion` which SPLITS data:
///    - Structure → Ion (for Storyline)
///    - Text → ctx.text_accumulator (for Content)
/// 4. Packages results into three KFX fragments
///
/// The Assembler knows about KFX Entity topology but NOT about element semantics.
/// Element semantics are handled by the Schema.
#[allow(dead_code)]
fn build_chapter_entities(
    chapter: &Chapter,
    chapter_id: ChapterId,
    section_name: &str,
    ctx: &mut ExportContext,
) -> Vec<KfxFragment> {
    use crate::formats::kfx::storyline::{ir_to_tokens, tokens_to_ion};

    let mut fragments = Vec::new();

    // =========================================================================
    // 1. SETUP: Naming for this chapter's entity triad
    // =========================================================================
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

    // =========================================================================
    // 2. GENERATE: Schema-driven token generation + text/structure split
    // =========================================================================
    // ir_to_tokens uses the Schema to convert IR → Tokens
    // tokens_to_ion SPLITS: Structure → Ion, Text → ctx.text_accumulator
    let tokens = ir_to_tokens(chapter, ctx);
    let storyline_content_list = tokens_to_ion(&tokens, ctx);

    // Drain the accumulated text strings (captured during tokens_to_ion)
    let content_strings = ctx.drain_text();

    // =========================================================================
    // 3. ASSEMBLE: Package into three KFX Entities
    // =========================================================================

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

/// Build the document symbols section.
///
/// This writes the local symbol table in the format expected by KFX readers:
/// ```ion
/// $ion_symbol_table::{
///   imports: [{ name: "YJ_symbols", version: 10, max_id: 851 }],
///   symbols: ["local_sym1", "local_sym2", ...]
/// }
/// ```
///
/// Ion system symbol IDs:
/// - $3 = $ion_symbol_table
/// - $4 = name
/// - $5 = version
/// - $6 = imports
/// - $7 = symbols
/// - $8 = max_id
///
/// IMPORTANT: The symbols in the list must appear in the exact same order
/// they were interned, so that symbol ID = KFX_SYMBOL_TABLE_SIZE + index.
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

/// Prepare a media asset's bytes for KFX bundling. Raster images are re-encoded
/// as **grayscale JPEG-XR** — the device is B&W e-ink and the source EPUB keeps
/// the color master, so this matches Amazon's own KFX image codec and shrinks
/// EPUB-sourced books toward the JXR size class. SVG assets are rasterized
/// first (KFX has no vector resource format — raw SVG bytes render as nothing
/// on device) and then take the same JXR path. Undecodable assets (fonts) and
/// any encode failure fall back to the JPEG sanitize path, which itself passes
/// non-image bytes through unchanged.
fn encode_asset_for_kfx(data: &[u8], mode: jxr::ColorMode) -> Vec<u8> {
    if let Some(jxr) = encode_jxr_asset(data, mode) {
        return jxr;
    }
    if let Some(img) = crate::image::svg::rasterize(data)
        && let Some(jxr) = encode_dynimg_jxr(&img, mode)
    {
        return jxr;
    }
    crate::image::jpeg::sanitize_for_kfx(data).unwrap_or_else(|| data.to_vec())
}

/// Decode a raster image and re-encode it as JPEG-XR in the requested
/// [`ColorMode`][jxr::ColorMode]: `Grayscale` (luma plane,
/// `8bppGray`) or `Color` (RGB planes, `24bppRGB`; channels identical everywhere
/// collapse to grayscale via the encoder's auto-detect). `None` if the bytes
/// aren't a decodable raster or exceed the encoder's range.
fn encode_jxr_asset(data: &[u8], mode: jxr::ColorMode) -> Option<Vec<u8>> {
    let img = ::image::load_from_memory(data).ok()?;
    encode_dynimg_jxr(&img, mode)
}

/// Encode an already-decoded raster as JPEG-XR in the requested
/// [`ColorMode`][jxr::ColorMode]. Shared by [`encode_jxr_asset`] (which decodes
/// bytes first) and the fixed-layout manga thumbnailer (which downscales a
/// `DynamicImage` before encoding).
fn encode_dynimg_jxr(img: &::image::DynamicImage, mode: jxr::ColorMode) -> Option<Vec<u8>> {
    use jxr::{ColorMode, ImageInput, encode};
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 || w > (1 << 16) || h > (1 << 16) {
        return None;
    }
    // Flatten alpha over white first, matching the JPEG path's
    // `flatten_to_rgb`. JPEG-XR itself can carry alpha (T.832 alpha image
    // plane; the codec crate implements it), but the plate formats emitted
    // here don't: 8bppGray (the only JXR Amazon ships — real KFX has no
    // color JXR at all) and 24bppRGB (our own color-mode extension). And
    // `to_luma8`/`to_rgb8` DROP the channel without compositing — a
    // transparent-background GIF/PNG would render as a black slab
    // (transparent pixels store (0,0,0)). See `flatten_alpha_over_white`
    // for why white is the right flatten color.
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
        // Only the gray/RGB raster modes are wired here (all the KFX
        // pipeline produces TODAY — gray default, Color when color KFX
        // lands). If a new ColorMode ever reaches this site, extend the
        // match; the debug_assert makes that loud instead of silently
        // dropping the asset.
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

/// Build font entity fragments ($262) from @font-face rules.
///
/// Font entities link font_family names (e.g., "cover-Ubuntu") to resource locations.
/// This enables Kindle to properly render custom fonts.
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

                // Search for matching resource. Pick the lexicographically
                // smallest match so a basename shared across directories
                // resolves deterministically, not in HashMap order.
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

        // Use original font family name (no "cover-" prefix)
        // This matches how styles reference fonts and is source-faithful
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

/// Build anchor fragments ($266) for all recorded anchors.
///
/// Returns (fragments, anchor_ids_by_fragment) where anchor_ids_by_fragment
/// maps fragment_id → list of anchor symbol IDs for use in position_map.
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

        // External anchors use uri instead of position
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

// ============================================================================
// Navigation Maps ($264, $265, $550)
// ============================================================================

/// Build position_map fragment ($264).
///
/// Maps each section to the list of EIDs it contains. This enables
/// the Kindle reader to track which section contains a given position.
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

/// One section's reading-position layout: its name (used to key the
/// `section_position_id_map` entity) + symbol (used inside `position_id_map`),
/// and its EIDs in reading order paired with each EID's span (advance in pids).
/// The first EID is the section root (page-template / container, span 1); the
/// rest are content EIDs (span = their UTF-16 text length, 1 for images).
struct SectionPos {
    name: String,
    sym: u64,
    /// `(eid, span)` in reading order, section root first.
    eids: Vec<(u64, i64)>,
}

/// Assemble the per-section position layout — the single source of truth for
/// `position_id_map`, `section_position_id_map`, and the navigable section
/// roots. Same section order as the position_map (standalone cover first if
/// present, then spine chapters by fragment id). `section_names` is the
/// section-name string per `section_ids` slot, needed to key the
/// `section_position_id_map` entities (Amazon keys them by section name).
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

    // Spine chapters, in fragment-id (spine) order — the cover, when it's the
    // first spine doc rather than a synthesized standalone, falls out here.
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

/// position_id_map ($265), Amazon's **section-keyed** reflowable shape:
/// `{contains: [{section_name, pid, length}, …]}`. `pid` is the section's
/// cumulative start; `length` is its span (which the paired
/// `section_position_id_map` terminator must agree with). This replaces the
/// dense `{eid, pid}` form — section roots are absent from that, so the device
/// can't resolve a TOC/landmark target that points at one (the cover's), which
/// is why the cover wouldn't jump. The reader decodes either shape
/// (`pid_map_from_book` → `fixed_layout_pid_map`).
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
/// position→EID walk (each element advances the running pid by the PREVIOUS
/// EID's span and names the current EID — a bare int when it's the previous + 1,
/// else `[advance, eid]`; an `[advance, 0]` terminator lands at the section
/// length). This is the fine index that makes section-level targets (the cover
/// section root) navigable on-device. Same encoding as
/// [`build_pdf_section_position_id_map_fragments`]; like it, each entity MUST be
/// keyed by the section-name symbol (else it serializes to id $0 and the device
/// rejects the book).
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

/// Build location_map fragment ($550).
///
/// Maps location numbers to positions. Each content block gets one entry
/// at offset 0 (matching Amazon's format for this entity).
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

/// Build resource_path fragment ($395).
///
/// This entity lists additional resource paths. For simple conversions,
/// the entries array is empty.
fn build_resource_path_fragment() -> KfxFragment {
    let ion = IonValue::Struct(vec![(KfxSymbol::Entries as u64, IonValue::List(vec![]))]);
    KfxFragment::singleton(KfxSymbol::ResourcePath, ion)
}

/// Build container_entity_map fragment ($419).
///
/// Lists all entities in the container for the reader to enumerate, plus an
/// `entity_dependencies` graph that tells Kindle how sections reach their
/// image data: section → external_resource → bcRawMedia location.
fn build_container_entity_map_fragment(
    container_id: &str,
    fragments: &[KfxFragment],
    ctx: &ExportContext,
) -> KfxFragment {
    // Collect all non-singleton entity name symbols (including bcRawMedia
    // location strings — Kindle requires these so it can resolve resource
    // dependencies).
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

    // Build entity_dependencies: section → [resource short names], and
    // external_resource → ['resource/<name>'] (the bcRawMedia symbol).
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

    KfxFragment::singleton(KfxSymbol::ContainerEntityMap, ion)
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

/// Check if a path is a font asset. Fonts are always bundled — they're
/// referenced by `@font-face` rules (matched fuzzily in
/// `build_font_fragments`), not by section image refs, so the
/// referenced-resource pruning must not drop them.
fn is_font_asset(path: &std::path::Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    matches!(
        ext.to_lowercase().as_str(),
        "ttf" | "otf" | "woff" | "woff2"
    )
}

/// Resolve landmarks from the Book's IR to fragment IDs.
///
/// This uses the parsed landmarks from the source format (EPUB, KFX, etc.)
/// to populate landmark_fragments in the context.
///
/// Handles both chapter-level targets (e.g., `chapter.xhtml`) and anchor-level
/// targets (e.g., `chapter.xhtml#section1`) by using ResolvedLinks.
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
            // Resolve the landmark's href using the book's navigation resolver
            // (a dead fragment on a real chapter falls back to the chapter start
            // rather than dropping the landmark).
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
                // Only add if not already present (first wins)
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

// ============================================================================
// Fixed-layout image manga → KFX (yj_non_pdf_fixed_layout)
//
// A comic/manga EPUB is pre-paginated: one image per spine page, no reflowable
// text. Amazon's KFX for such a book (e.g. その着せ替え人形は恋をする) is a
// `yj_non_pdf_fixed_layout` + `yj_double_page_spread` document — every page is a
// fixed-size, scale-to-fit image; consecutive pages pair into right-to-left
// facing spreads; each full image carries a downscaled thumbnail
// (`yj_thumbnails_present`) for the device's page scrubber. The reflow emitter
// (`build_kfx_container`) would flatten this into one scrolling column, so we
// branch here when `metadata.fixed_layout` is set and every spine page is a lone
// image (see [`is_fixed_layout_image_book`]).
//
// Structure, mirroring the reference KFX entity-for-entity:
//   - cover  = its own section; the page_template carries the page box +
//              scale_fit + float:center, and the storyline is the bare cover
//              image (no wrapping container).
//   - spread = one section per 1–2 consecutive pages; page_template
//              layout:page_spread + virtual_panel; the storyline holds one
//              scale_fit/float:center page container per page, each an inner
//              vertical container (style `sJ`) wrapping the page image
//              (style `sG` = the uniform page box, centered).
//   - document_data: writing_mode horizontal_tb + direction rtl (the device's
//     RTL page-turn signal for a fixed-layout book — reading_orders' $425 is
//     device-ignored; see reference_kfx_device_rtl_writing_mode).
// ============================================================================

/// Downscaled page-thumbnail box (Amazon uses ~270×384 for a portrait manga
/// page; `yj_thumbnails_present`). Aspect is preserved, so a thumbnail fits
/// inside this box rather than filling it exactly.
const MANGA_THUMB_W: u32 = 270;
const MANGA_THUMB_H: u32 = 384;

/// True when the book should be emitted as a fixed-layout image manga rather
/// than reflowed: the metadata marks it fixed-layout AND every spine page is a
/// single image (an SVG-wrapped or bare `<img>` page). A reflowable book, or a
/// fixed-layout book carrying real text, falls through to `build_kfx_container`.
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

/// One reading unit = one section + one storyline: the cover (a solo page) or a
/// right-to-left facing spread of 1–2 pages.
struct MangaUnit {
    section_name: String,
    section_sym: u64,
    story_sym: u64,
    /// EID of the section's single page_template container.
    pt_id: u64,
    is_cover: bool,
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
/// caller has already confirmed [`is_fixed_layout_image_book`].
fn image_fxl_to_kfx(
    book: &mut Book,
    on_progress: &dyn Fn(&str, usize, usize, &str),
) -> io::Result<Vec<u8>> {
    let container_id = generate_container_id(&book.metadata().title);
    let mut ctx = ExportContext::new();

    // Book-level layout signals. A manga is horizontal_tb text with an rtl page
    // turn; `document_direction` resolves rtl from the spine's
    // page-progression-direction. Both are stamped into document_data and the
    // reading order (reference_kfx_device_rtl_writing_mode).
    let writing_mode = book_writing_mode(book);
    let direction = document_direction(book, writing_mode);
    ctx.document_writing_mode = writing_mode;
    // FXL/image path: no reflowable text styles to override, so the per-style
    // baseline tracks the document mode (no suppression needed).
    ctx.style_writing_mode_baseline = writing_mode;
    ctx.document_direction = direction;
    let ppd_sym = Some(direction);

    let color_mode = book.image_color_mode();
    let language = book.metadata().language.clone();
    let mut box_dims = book.metadata().default_viewport;

    // ---- Load the per-page image href for every spine page ----
    let mut spine_ids: Vec<_> = book.spine().iter().map(|e| e.id).collect();
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
    // file name) so a `images/x.jpg` src still finds its bytes.
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
    // very same image as the first content page (spine[1]), so keeping both
    // shows the cover twice and offsets every facing spread by one. Amazon's KPR
    // removes it too (`num-dual-covers-removed`). Detected by identical resolved
    // image paths; the real first page becomes the solo cover.
    if spine_ids.len() >= 2
        && let (Some(a), Some(b)) = (resolve(&hrefs[0]), resolve(&hrefs[1]))
        && a == b
    {
        spine_ids.remove(0);
        hrefs.remove(0);
    }
    let n = spine_ids.len();

    // Register the cover page's image in the resource registry so the reflowable
    // `build_book_metadata_fragment` resolves the library-tile / sleep-screen
    // cover. Page 0 is `e0` under both the registry's hex naming and our own
    // `e{i}` scheme, so the metadata reference matches the actual resource
    // entity. Interior pages use our `e{i}` names directly and need no registry
    // entry.
    if let Some(cover_href) = hrefs.first() {
        ctx.resource_registry.register(cover_href, &mut ctx.symbols);
        let _ = ctx.resource_registry.get_or_create_name(cover_href); // -> "e0"
    }

    let mut encs: Vec<MangaEnc> = Vec::with_capacity(n);
    for (i, href) in hrefs.iter().enumerate() {
        on_progress("images", i + 1, n, "Encoding pages");
        let path = resolve(href).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("fixed-layout page image not found: {href}"),
            )
        })?;
        let raw = book.load_asset(&path)?;
        let (w, h) = crate::util::extract_image_dimensions(&raw).unwrap_or((0, 0));
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
    }
    // Fallback page box when no viewport was declared: the largest page image.
    if box_dims.is_none() {
        let mx = encs.iter().map(|e| e.w).max().unwrap_or(0);
        let my = encs.iter().map(|e| e.h).max().unwrap_or(0);
        if mx > 0 && my > 0 {
            box_dims = Some((mx, my));
        }
    }
    let (box_w, box_h) = box_dims.unwrap_or((1, 1));

    // Shared image styles (the reference KFX's `sJ` inner-container fill and
    // `sG` page-box image placement), interned once up front.
    let sj_sym = ctx.symbols.get_or_intern("sJ");
    let sg_sym = ctx.symbols.get_or_intern("sG");

    // ---- Group pages into units, allocate section / story / EIDs ----
    let groups = manga_page_groups(n);

    // Spine page index → its image EID (nav targets) and spine ChapterId → page
    // index (TOC target resolution), filled as units are built.
    let mut page_image_id = vec![0u64; n];
    let chapter_to_index: std::collections::HashMap<ChapterId, usize> = spine_ids
        .iter()
        .enumerate()
        .map(|(i, &id)| (id, i))
        .collect();

    let mut units: Vec<MangaUnit> = Vec::with_capacity(groups.len());
    let mut any_spread = false;
    let mut any_thumb = false;
    for (u, group) in groups.iter().enumerate() {
        let section_name = format!("c{u}");
        let section_sym = ctx.register_section(&section_name);
        let story_sym = ctx.symbols.get_or_intern(&format!("story_c{u}"));
        let is_cover = u == 0;
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
            // EIDs: cover = bare image; content = outer→inner→image.
            let (outer_id, inner_id, image_id) = if is_cover {
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
            is_cover,
            pages,
        });
    }

    // ---- Synthesis (reference entity order) ----
    let mut fragments: Vec<KfxFragment> = Vec::new();

    // 1. content_features ($585)
    fragments.push(build_manga_content_features_fragment(any_spread, any_thumb));
    // 2. book_metadata ($490) — reuse the reflowable builder (reads Book metadata)
    fragments.push(build_book_metadata_fragment(book, &container_id, &ctx));
    // 3. metadata ($258) — reading order + page-progression-direction
    fragments.push(build_pdf_metadata_fragment(&ctx, ppd_sym));
    // 4. document_data ($538) — inserted here later, once max_id is known.
    let document_data_index = fragments.len();

    // 5. sections ($260) + 6. storylines ($259). The solo cover's page_template
    // is sized to the cover image's own dimensions (shown full-screen, no
    // letterbox); content pages share the uniform page box so facing pages align.
    let mut sections = Vec::with_capacity(units.len());
    let mut storylines = Vec::with_capacity(units.len());
    for unit in &units {
        let cover_dims = unit
            .is_cover
            .then(|| unit.pages.first())
            .flatten()
            .map(|p| &encs[p.enc])
            .filter(|e| e.w > 0 && e.h > 0)
            .map(|e| (e.w, e.h));
        let (sw, sh) = cover_dims.unwrap_or((box_w, box_h));
        sections.push(build_manga_section(unit, sw, sh));
        storylines.push(build_manga_storyline(unit, box_w, box_h, sj_sym, sg_sym));
    }
    fragments.extend(sections);
    fragments.extend(storylines);

    // 7. styles ($157) — sJ inner fill + sG page-box image placement
    fragments.push(build_manga_style_sj(sj_sym, &language));
    fragments.push(build_manga_style_sg(sg_sym, box_w, box_h));

    // 8. external_resource ($164) — full images (with `thumbnails` link) then thumbs
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

    // 10. navigation — referenced nav_containers (a `landmarks` cover_page → the
    // cover's page_template, and a `toc` mapping the source table of contents to
    // page image EIDs) + a thin book_navigation. A fixed-layout book rejects an
    // inline nav_container, so this mirrors the PDF path's referenced form
    // (feedback_kfx_nav_device_strict). `resolve_links` populates the TOC entry
    // targets; a failure just yields a toc-less (landmarks-only) nav.
    let _ = book.resolve_links();
    let toc = book.toc().to_vec();
    fragments.extend(build_manga_nav_fragments(
        &units,
        &toc,
        &page_image_id,
        &chapter_to_index,
        &mut ctx,
    ));

    // 11. position system — section-keyed, so nav targets resolve on-device.
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

    // document_data now that every EID is allocated (max_id correct).
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

/// Partition `n` spine pages into reading units: page 0 is the cover (shown
/// solo), and the rest pair into consecutive facing spreads with an odd tail
/// page standing alone — matching the reference manga's cover + consecutive-pair
/// + solo-tail layout. Returns page-index groups (`[[0], [1,2], [3,4], …]`).
fn manga_page_groups(n: usize) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    if n == 0 {
        return groups;
    }
    groups.push(vec![0]);
    let mut i = 1;
    while i < n {
        if i + 1 < n {
            groups.push(vec![i, i + 1]);
            i += 2;
        } else {
            groups.push(vec![i]);
            i += 1;
        }
    }
    groups
}

/// Downscale a page image to a thumbnail (aspect preserved, fit within
/// [`MANGA_THUMB_W`]×[`MANGA_THUMB_H`]) and JXR-encode it. `None` if the bytes
/// aren't a decodable raster or the encoder rejects them — the page then ships
/// without a thumbnail.
fn make_manga_thumbnail(data: &[u8], mode: jxr::ColorMode) -> Option<(Vec<u8>, u32, u32)> {
    let img = ::image::load_from_memory(data).ok()?;
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
/// `yj_non_pdf_fixed_layout` (v2), plus `yj_double_page_spread` when any unit is
/// a real 2-page spread and `yj_thumbnails_present` when any page carries a
/// thumbnail.
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
    let reading_order = pdf_reading_order(ctx, ppd_sym);
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

/// section ($260) for a manga unit. The cover's page_template carries the page
/// box + scale_fit (shown solo, full-screen); a content unit's page_template is
/// `layout:page_spread` (its storyline's page containers hold the dimensions).
/// Both enable `virtual_panel` (Kindle Panel View).
fn build_manga_section(unit: &MangaUnit, box_w: u32, box_h: u32) -> KfxFragment {
    let template = if unit.is_cover {
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

/// storyline ($259) for a manga unit. The cover storyline is the bare cover
/// image; a content unit's storyline holds one page container per page — an
/// outer scale_fit/float:center box (sized to the page box) wrapping an inner
/// vertical container (style `sJ`) that holds the page image (style `sG`).
fn build_manga_storyline(
    unit: &MangaUnit,
    box_w: u32,
    box_h: u32,
    sj_sym: u64,
    sg_sym: u64,
) -> KfxFragment {
    let content: Vec<IonValue> = if unit.is_cover {
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
        (KfxSymbol::Width as u64, pdf_percent_100()),
        (KfxSymbol::Height as u64, pdf_percent_100()),
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
/// top-left origin, centered (so a smaller page image is centered within the
/// spread box rather than pinned to a corner).
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

/// Navigation for a fixed-layout manga, in the referenced form the device
/// requires: a `landmarks` nav_container (cover_page → the cover's
/// page_template, so the device opens on a full-screen cover), a `toc`
/// nav_container mapping the source table of contents onto page image EIDs (when
/// any entry resolves), and a thin `book_navigation` referencing them by name.
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
    // image EID (which `position_id_map` registers, so the device resolves it).
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
/// target doesn't resolve to a page is dropped (with its subtree), so a partial
/// TOC still yields the entries that do resolve.
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
        if unit.is_cover {
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

/// section_position_id_map ($609): one entity per section walking its EIDs (each
/// span 1), keyed by the section-name symbol (as Amazon does) so the device can
/// address it. A bare int continues from the previous EID + 1; else `[advance,
/// eid]`; an `[1, 0]` terminator lands at `pid == length`.
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
    KfxFragment::singleton(KfxSymbol::ContainerEntityMap, ion)
}

// ============================================================================
// PDF → KFX (fixed-layout, PDF-backed PDOC)
//
// This is NOT a content conversion. The PDF is embedded verbatim as a single
// `bcRawMedia` and the *device* renders each page (which is what lets the
// Scribe pen draw over it). We author only the thin fixed-layout skeleton:
// one section/storyline/external_resource per page, all referencing the shared
// PDF blob with a `page_index`, plus PDOC metadata and the `yj_pdf_support` /
// `yj_fixed_layout` feature flags.
//
// P0 scope (round-trip proof): embed + skeleton only. Deferred to later phases:
//   - portrait+landscape page_template pair (needs an SExp IonValue variant for
//     the `condition:(isPortrait)` s-expression) — P1
//   - page-1 cover render — P2
//   - selectable text layer + per-word auxiliary_data — P3
// ============================================================================

/// Metadata stamped into a PDF→KFX (PDOC) conversion.
pub struct PdfKfxMeta {
    pub title: String,
    pub author: Option<String>,
    pub language: String,
    /// Publication date — any of `YYYY`, `YYYY-MM`, `YYYY-MM-DD`, or a full ISO
    /// timestamp. Emitted as the `issue_date` title-metadata entry (truncated to
    /// the date part). `None` → no `issue_date` (optional for a PDOC; Amazon
    /// stamps the send date there, we stamp the work's date when known).
    pub date: Option<String>,
    /// Publisher imprint. Emitted as the `publisher` entry; `None`/blank yields
    /// the empty value Amazon's PDOC also carries.
    pub publisher: Option<String>,
    /// Page progression direction — `Some("rtl")` turns pages right-to-left
    /// (Japanese/manga), `Some("ltr")` left-to-right, `None` omits it (device
    /// default, ltr). A scanned/text PDF carries no such hint, so a Japanese
    /// book must set this explicitly (e.g. from edited library metadata) and be
    /// force-reconverted. Applied to every reading order's
    /// `page_progression_direction` ($425) and to `document_data.direction`
    /// ($192) — see `build_pdf_metadata_fragment` /
    /// `build_pdf_document_data_fragment`.
    pub page_progression_direction: Option<String>,
}

/// Per-page bookkeeping gathered in the survey pass.
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
    /// Text layer for this page (empty when no text was extracted). The
    /// secondary "text" storyline's name symbol, the EID of the `{story_name,
    /// ignore}` child that pulls it into the page container, the page's
    /// `links_extracted` aux symbol, and one record per extracted run. All zero
    /// / empty when `runs` is empty — emission is gated on `!runs.is_empty()`.
    text_story_sym: u64,
    text_ref_id: u64,
    links_aux_sym: u64,
    runs: Vec<PdfRunRec>,
}

/// Per-text-run bookkeeping: the text item's EID, the symbol naming its
/// `text_baseline` auxiliary_data entity, and the run's UTF-16 length (its span
/// in the section position map — each character is one reading position).
struct PdfRunRec {
    id: u64,
    baseline_aux_sym: u64,
    len: usize,
}

/// Symbolic name of the single shared PDF raw-media resource.
const PDF_RSRC_NAME: &str = "rsrc0";

/// Symbolic name of the optional page-1 cover JPEG resource.
const COVER_RSRC_NAME: &str = "ecover";

/// Convert a probed PDF into a fixed-layout, PDF-backed KFX (PDOC).
///
/// `cover_jpeg`, when present, is embedded as a loose JPEG resource referenced
/// by `book_metadata.cover_image` — the library tile / PDOC sleep-screen art
/// (keyed by the synthesized ASIN). It is *not* a reading-order page; the PDF
/// pages remain the only sections. Render it with [`crate::formats::pdf::render`]. When
/// `None` (e.g. the PDF engine is unavailable on a non-macOS build), the KFX is
/// cover-less but otherwise identical — the embedded PDF is unaffected.
///
/// `text`, when present, is the per-page selectable text layer extracted by
/// [`crate::formats::pdf::render::extract_pdf_text`]. Each page with runs gets a second,
/// **invisible** "text" storyline (positioned, word-segmented) pulled into the
/// page container, plus the `auxiliary_data` + capability flags that make the
/// fixed-layout text live on device (select / search / dictionary / highlight) —
/// matching Amazon's Send-to-Kindle structure. When `None`/empty for a page, that
/// page stays visual-only (e.g. a scanned/image page, or a non-macOS build).
pub fn pdf_to_kfx(
    pdf: &crate::import::pdf::PdfDoc,
    meta: &PdfKfxMeta,
    cover_jpeg: Option<&[u8]>,
    text: Option<&[crate::formats::pdf::render::PageText]>,
) -> Vec<u8> {
    let container_id = generate_container_id(&meta.title);
    let mut ctx = ExportContext::new();
    let n = pdf.pages.len();

    // Page-turn direction for the PDOC: resolve once to a $rtl/$ltr symbol (or
    // None to omit), then stamp it into both reading orders ($425) and
    // document_data.direction ($192) below.
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

        // Text layer: when this page has extracted runs, allocate the text
        // storyline's name, the `{story_name, ignore}` child EID, the page's
        // `links_extracted` aux symbol, and one EID + `text_baseline` aux symbol
        // per run (storing each run's UTF-16 length as its position span).
        let runs = page_runs(i);
        let (text_story_sym, text_ref_id, links_aux_sym, run_recs) = if runs.is_empty() {
            (0, 0, 0, Vec::new())
        } else {
            let tss = ctx.symbols.get_or_intern(&format!("tstory_c{i}"));
            let tref = ctx.next_fragment_id();
            let laux = ctx.symbols.get_or_intern(&format!("dp{i}"));
            let rrecs: Vec<PdfRunRec> = runs
                .iter()
                .enumerate()
                .map(|(j, run)| PdfRunRec {
                    id: ctx.next_fragment_id(),
                    baseline_aux_sym: ctx.symbols.get_or_intern(&format!("dt{i}_{j}")),
                    len: run.content.encode_utf16().count(),
                })
                .collect();
            (tss, tref, laux, rrecs)
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
            text_story_sym,
            text_ref_id,
            links_aux_sym,
            runs: run_recs,
        });
    }
    // Intern the shared raw-media location so the bcRawMedia entity resolves.
    ctx.symbols.get_or_intern(&raw_location);

    // Optional page-1 cover: a loose JPEG resource (external_resource +
    // bcRawMedia) referenced by `book_metadata.cover_image`. Survey its symbols
    // now so the entity map and metadata resolve. `(res_sym, width_px,
    // height_px)`.
    let cover: Option<(u64, u32, u32)> = cover_jpeg.map(|jpeg| {
        let res_sym = ctx.symbols.get_or_intern(COVER_RSRC_NAME);
        ctx.symbols.get_or_intern(&cover_location);
        let (w, h) = crate::util::extract_image_dimensions(jpeg).unwrap_or((0, 0));
        (res_sym, w, h)
    });

    // Any page with extracted runs makes the book's text live — gates the
    // `selection` / `yj_custom_word_iterator` capability flags.
    let has_text = recs.iter().any(|r| !r.runs.is_empty());

    // Resource-descriptor aux (Amazon's `d6`/`d7`): `d6` describes the embedded
    // PDF resource (every page's external_resource references it via
    // `auxiliary_data`), `d7` lists `[d6]`, and `document_data` points at `d7`.
    // Replicating this is part of full structural parity with Amazon's S2K KFX.
    let pdf_rsrc_desc_sym = ctx.symbols.get_or_intern("d6");
    let aux_list_sym = ctx.symbols.get_or_intern("d7");

    // ---- Synthesis: build fragments in reference entity order ----
    let mut fragments: Vec<KfxFragment> = Vec::new();

    // 1. content_features ($585)
    fragments.push(build_pdf_content_features_fragment(has_text));
    // 2. book_metadata ($490) — PDOC (with cover_image when a cover is present)
    fragments.push(build_pdf_book_metadata_fragment(
        meta,
        &container_id,
        pdf,
        cover.map(|_| COVER_RSRC_NAME),
        has_text,
    ));
    // 3. metadata ($258) — reading order
    fragments.push(build_pdf_metadata_fragment(&ctx, ppd_sym));
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
        if !rec.runs.is_empty() {
            text_storylines.push(build_pdf_text_storyline(rec, page_runs(i)));
        }
        resources.push(build_pdf_external_resource(
            rec,
            i,
            page.width,
            page.height,
            &raw_location,
            pdf_rsrc_desc_sym,
        ));
    }
    fragments.extend(sections);
    fragments.extend(storylines);
    fragments.extend(text_storylines);

    // auxiliary_data ($597) resource descriptors: `d6` describes the embedded
    // PDF (the single shared resource), `d7` lists `[d6]`. Amazon's
    // external_resources + document_data reference these. (Replaces bokai's old
    // per-section `IS_TARGET_SECTION` aux, which Amazon's PDF KFX does not have.)
    fragments.push(build_aux_fragment(
        "d6",
        pdf_rsrc_desc_sym,
        vec![
            (
                "resource_stream",
                IonValue::String(PDF_RSRC_NAME.to_string()),
            ),
            ("type", IonValue::String("resource".to_string())),
            ("size", IonValue::String(pdf.bytes.len().to_string())),
        ],
    ));
    fragments.push(build_aux_fragment(
        "d7",
        aux_list_sym,
        vec![(
            "auxData_resource_list",
            IonValue::List(vec![IonValue::Symbol(pdf_rsrc_desc_sym)]),
        )],
    ));

    // auxiliary_data ($597) for the text layer: per page a `links_extracted`
    // entry (referenced by the page container's `auxiliary_data.default`), and
    // per run a `text_baseline` entry (referenced by the text item's
    // `auxiliary_data.'yj.conversion'`). Mirrors Amazon's ~10.9k aux entities.
    for (i, rec) in recs.iter().enumerate() {
        if rec.runs.is_empty() {
            continue;
        }
        fragments.push(build_kv_aux_fragment(
            &format!("dp{i}"),
            rec.links_aux_sym,
            "links_extracted",
            IonValue::Bool(true),
        ));
        for (j, (rr, run)) in rec.runs.iter().zip(page_runs(i)).enumerate() {
            fragments.push(build_kv_aux_fragment(
                &format!("dt{i}_{j}"),
                rr.baseline_aux_sym,
                "text_baseline",
                IonValue::Int(run.baseline),
            ));
        }
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

    // Navigation: `nav_container` ($391) entities — a `page_list` (page-number
    // navigation, one entry per page) and, when the PDF has bookmarks, a `toc` —
    // plus a thin `book_navigation` ($389) that references them by name (Amazon's
    // PDF shape; the fixed-layout reader rejects an inline nav_container). Every
    // entry targets its page's image EID — the EID `position_id_map` registers,
    // so the device can resolve the nav position.
    fragments.extend(build_pdf_nav_fragments(pdf, &recs, &mut ctx));

    // Position system. For a fixed-layout book the reader resolves a
    // text-selection touch through `position_id_map` (section → pid range) +
    // `section_position_id_map` (per-section position → EID); both are required
    // for the overlay text to be selectable.
    fragments.push(build_pdf_position_map_fragment(&recs));
    fragments.push(build_pdf_position_id_map_fragment(&recs));
    fragments.extend(build_pdf_section_position_id_map_fragments(&recs));
    // NB: no `location_map` ($550) — Amazon's PDF KFX has none, and ours would
    // reference page `image_id`s that the section-keyed `position_id_map` no
    // longer maps to PIDs, leaving dangling refs the reader rejects.

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

    // document_data now that every fragment ID is allocated (max_id correct).
    fragments.insert(
        document_data_index,
        build_pdf_document_data_fragment(&ctx, aux_list_sym, ppd_sym),
    );

    // ---- Serialize ----
    let symtab_ion = build_symbol_table_ion(ctx.symbols.local_symbols());
    let format_caps_ion = build_format_capabilities_ion();
    let entities = serialize_fragments(&fragments, ctx.symbols.local_symbols());
    serialize_container(&container_id, &entities, &symtab_ion, &format_caps_ion)
}

/// A percent dimension struct: `{ value: 100, unit: percent }`.
fn pdf_percent_100() -> IonValue {
    IonValue::Struct(vec![
        (KfxSymbol::Value as u64, IonValue::Int(100)),
        (
            KfxSymbol::Unit as u64,
            IonValue::Symbol(KfxSymbol::Percent as u64),
        ),
    ])
}

/// Build the storyline ($259) for one PDF page: a page-sized container holding
/// the PDF page as a 100%×100% image (Amazon's `l2`). When the page has a text
/// layer (`rec.runs`), the container also carries an `auxiliary_data.default`
/// (`links_extracted`) marker and a second, invisible `{story_name, ignore}`
/// child that pulls in the page's text storyline as a positioned overlay.
fn build_pdf_page_storyline(rec: &PdfPageRec, width_pt: f32, height_pt: f32) -> KfxFragment {
    // Amazon sizes the page container in points×100.
    let fixed_w = (width_pt * 100.0).round() as i64;
    let fixed_h = (height_pt * 100.0).round() as i64;

    let image = IonValue::Struct(vec![
        (KfxSymbol::Id as u64, IonValue::Int(rec.image_id as i64)),
        (KfxSymbol::Width as u64, pdf_percent_100()),
        (KfxSymbol::Height as u64, pdf_percent_100()),
        (
            KfxSymbol::Type as u64,
            IonValue::Symbol(KfxSymbol::Image as u64),
        ),
        (
            KfxSymbol::ResourceName as u64,
            IonValue::Symbol(rec.res_sym),
        ),
    ]);

    // Container content: the PDF page image, plus (for a text page) the
    // text-storyline reference marked `ignore: true` (invisible overlay).
    let mut content = vec![image];
    if !rec.runs.is_empty() {
        content.push(IonValue::Struct(vec![
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
        ]));
    }

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
    // Amazon puts `auxiliary_data: {default: d…}` on the page container (before
    // `type`) when there's a text layer.
    if !rec.runs.is_empty() {
        container_fields.push((
            KfxSymbol::AuxiliaryData as u64,
            IonValue::Struct(vec![(
                KfxSymbol::Default as u64,
                IonValue::Symbol(rec.links_aux_sym),
            )]),
        ));
    }
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

/// Build the invisible "text" storyline ($259) for one PDF page — the
/// selectable overlay (Amazon's `l1SJ`). Each extracted run becomes a
/// `type: text` item positioned at fixed `top`/`left` with `visibility: false`
/// in its `style_events`, word-segmented for the custom word iterator, and
/// linked to its `text_baseline` aux entry. The page-image storyline pulls this
/// in by `story_name` (see [`build_pdf_page_storyline`]).
fn build_pdf_text_storyline(
    rec: &PdfPageRec,
    runs: &[crate::formats::pdf::render::TextRun],
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
                    KfxSymbol::AuxiliaryData as u64,
                    IonValue::Struct(vec![(
                        KfxSymbol::YjConversion as u64,
                        IonValue::Symbol(rr.baseline_aux_sym),
                    )]),
                ),
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
fn build_kv_aux_fragment(fid: &str, kfx_id_sym: u64, key: &str, value: IonValue) -> KfxFragment {
    build_aux_fragment(fid, kfx_id_sym, vec![(key, value)])
}

/// Build the section ($260) for one PDF page. Emits Amazon's portrait+landscape
/// `page_template` pair, both referencing the same storyline, selected on device
/// by the `condition: (isPortrait)` / `(isLandscape)` s-expression:
/// - portrait:  `{ id, story_name, condition:(isPortrait),  layout:vertical }`
/// - landscape: `{ id, width:100%, story_name, fixed_width:100%,
///                 condition:(isLandscape), layout:overflow }`
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
        (KfxSymbol::Width as u64, pdf_percent_100()),
        (KfxSymbol::StoryName as u64, IonValue::Symbol(rec.story_sym)),
        (KfxSymbol::FixedWidth as u64, pdf_percent_100()),
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

/// Build the external_resource ($164) for one PDF page: a `format: pdf` view of
/// the shared blob at `page_index`, sized to the page. References the shared
/// resource descriptor (`d6`) via `auxiliary_data` and carries `margin: 0`, both
/// matching Amazon's per-page resources.
fn build_pdf_external_resource(
    rec: &PdfPageRec,
    page_index: usize,
    width_pt: f32,
    height_pt: f32,
    raw_location: &str,
    rsrc_desc_sym: u64,
) -> KfxFragment {
    let ion = IonValue::Struct(vec![
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
        (
            KfxSymbol::AuxiliaryData as u64,
            IonValue::Symbol(rsrc_desc_sym),
        ),
        (
            KfxSymbol::ResourceWidth as u64,
            IonValue::Int(width_pt.round() as i64),
        ),
        (
            KfxSymbol::ResourceHeight as u64,
            IonValue::Int(height_pt.round() as i64),
        ),
        (
            KfxSymbol::ResourceName as u64,
            IonValue::Symbol(rec.res_sym),
        ),
        (KfxSymbol::Margin as u64, IonValue::Int(0)),
    ]);
    KfxFragment::new(KfxSymbol::ExternalResource, format!("e{page_index}"), ion)
}

/// Build the external_resource ($164) for the page-1 cover JPEG: a loose image
/// resource (a real JPEG, no `page_index`) referenced by
/// `book_metadata.cover_image`. Unlike the page resources it is not part of any
/// section — it exists only to give the library tile / sleep screen its art.
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

/// content_features ($585) for a PDF-backed fixed-layout book. Mirrors Amazon's
/// set minus `yj_custom_word_iterator` (which needs the P3 text layer).
fn build_pdf_content_features_fragment(has_text: bool) -> KfxFragment {
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
    let ion = IonValue::Struct(vec![(KfxSymbol::Features as u64, IonValue::List(feats))]);
    KfxFragment::singleton(KfxSymbol::ContentFeatures, ion)
}

/// book_metadata ($490) for a PDOC, mirroring "Send to Kindle" categories.
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
    // The cover_image value is the cover resource's symbolic name (a string the
    // device matches against an external_resource), mirroring the EPUB path.
    // PDOC + a synthesized ASIN is what makes the Kindle render the tile from
    // this embedded image (see reference_kfx_asin_pdoc_cover).
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

/// Deterministic content_id/ASIN for a PDOC — in the SAME 32-char Crockford-style
/// base32 shape as every other sideload, via the single canonical
/// [`crate::formats::kfx::metadata::generate_content_id`]. (Previously this rolled its own
/// 32-hex value, so a PDF→KFX baked a *different alphabet* than `resolve_export_asin`
/// recomputes — `books.asin` never matched the on-device `.sdr`/`.notebooks` key.
/// One fabricator now.) Seeded by the PDF's stable identity (title + author + byte
/// size + page count) since a PDF carries no publication identifier, so
/// re-converting the same PDF yields the same id.
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
fn ppd_symbol(ppd: Option<&str>) -> Option<KfxSymbol> {
    match ppd {
        Some("rtl") => Some(KfxSymbol::Rtl),
        Some("ltr") => Some(KfxSymbol::Ltr),
        _ => None,
    }
}

/// Build a default reading order over all sections, appending the
/// `page_progression_direction` ($425) symbol when `ppd_sym` is set.
fn pdf_reading_order(ctx: &ExportContext, ppd_sym: Option<KfxSymbol>) -> IonValue {
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
fn build_pdf_metadata_fragment(ctx: &ExportContext, ppd_sym: Option<KfxSymbol>) -> KfxFragment {
    let ion = IonValue::Struct(vec![(
        KfxSymbol::ReadingOrders as u64,
        IonValue::List(vec![pdf_reading_order(ctx, ppd_sym)]),
    )]);
    KfxFragment::singleton(KfxSymbol::Metadata, ion)
}

/// document_data ($538): minimal fixed-layout document — max_id, pan_zoom,
/// `auxiliary_data: {'yj.authoring': d7}` (the resource-descriptor list), and the
/// reading order. (Reflow fields like font_size/line_height are irrelevant to a
/// PDF-backed book and omitted, matching Amazon's PDF document_data.)
fn build_pdf_document_data_fragment(
    ctx: &ExportContext,
    aux_list_sym: u64,
    ppd_sym: Option<KfxSymbol>,
) -> KfxFragment {
    let reading_order = pdf_reading_order(ctx, ppd_sym);
    let mut fields = vec![
        (KfxSymbol::MaxId as u64, IonValue::Int(ctx.max_eid() as i64)),
        (
            KfxSymbol::PanZoom as u64,
            IonValue::Symbol(KfxSymbol::Enabled as u64),
        ),
        (
            KfxSymbol::AuxiliaryData as u64,
            IonValue::Struct(vec![(
                KfxSymbol::YjAuthoring as u64,
                IonValue::Symbol(aux_list_sym),
            )]),
        ),
        (
            KfxSymbol::ReadingOrders as u64,
            IonValue::List(vec![reading_order]),
        ),
    ];
    // Mirror the page progression onto `document_data.direction` ($192) too. The
    // reflow path hardcodes this to ltr and signals rtl via writing_mode, but a
    // fixed-layout PDOC has no writing_mode, so the direction field is the
    // document-level signal that pairs with the reading order's $425.
    if let Some(sym) = ppd_sym {
        fields.push((KfxSymbol::Direction as u64, IonValue::Symbol(sym as u64)));
    }
    KfxFragment::singleton(KfxSymbol::DocumentData, IonValue::Struct(fields))
}

/// position_map ($264): one entry per page enumerating the section's content
/// EIDs. For a text page that includes the text-ref child + every text item, so
/// the device registers the text positions (Amazon does this via a
/// `[content_start, count]` run covering image..last-text-item).
/// The number of reading positions a section spans — one each for the portrait
/// page_template, container, image, the optional text-ref, and the landscape
/// page_template, plus one per character (UTF-16 unit) of every text run. Drives
/// both `position_id_map`'s section `length` and the `section_position_id_map`
/// terminator, which must agree. Keep in sync with
/// [`build_pdf_section_position_id_map_fragments`].
fn pdf_section_span(rec: &PdfPageRec) -> i64 {
    let mut span = 4i64; // pt_id + container + image + pt_landscape
    if !rec.runs.is_empty() {
        span += 1; // text_ref
        span += rec.runs.iter().map(|r| r.len as i64).sum::<i64>();
    }
    span
}

/// position_map ($264): one entry per page enumerating the section's EIDs — both
/// page templates, container, image, and (for a text page) the text-ref child +
/// every text item. Tells the device which EIDs belong to each section.
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
            if !rec.runs.is_empty() {
                ids.push(IonValue::Int(rec.text_ref_id as i64));
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
/// [{section_name, pid, length}, …]}` — the **section-keyed** shape the
/// fixed-layout reader needs (NOT the reflowable `{eid, pid}` form, which leaves
/// the overlay text unselectable). `pid` is the section's cumulative start
/// position; `length` is its span. Paired with `section_position_id_map`, this is
/// what makes the text live.
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
/// positions to EIDs within that section — the fine index the fixed-layout reader
/// resolves a text-selection touch through (absent ⇒ no selection). The compact
/// `contains` walk, decoded from Amazon's S2K KFX: each element advances the
/// running pid by the PREVIOUS EID's span and names the current EID — a bare int
/// when that EID is the previous + 1, else `[advance, eid]`. Span is 1 for the
/// page templates / container / image / text-ref and the run's UTF-16 length for
/// a text item; an `[advance, 0]` terminator lands at `pid == length` (agreeing
/// with `position_id_map`).
fn build_pdf_section_position_id_map_fragments(recs: &[PdfPageRec]) -> Vec<KfxFragment> {
    recs.iter()
        .map(|rec| {
            // (eid, span) in reading-position order: the portrait page_template
            // opens the section, the landscape page_template closes it — Amazon's
            // section "anchors" are exactly these page_template EIDs (real, backed
            // elements; inventing fresh anchor EIDs gives dangling references the
            // device rejects with "An error occurred").
            let mut order: Vec<(u64, i64)> =
                vec![(rec.pt_id, 1), (rec.container_id, 1), (rec.image_id, 1)];
            if !rec.runs.is_empty() {
                order.push((rec.text_ref_id, 1));
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
            // Key the entity by the SECTION NAME symbol — exactly as Amazon does
            // (the `section` and its `section_position_id_map` share the
            // section-name symbol as their entity id). Using a distinct
            // `spid_*` name was never interned, so `serialize_fragments` fell
            // through to id $0 (kProperty_Invalid): the device couldn't address
            // any section's position map by name, so every text-layer build was
            // rejected ("An error occurred") while embed-only builds (which have
            // no section_position_id_map) opened fine.
            KfxFragment::new(
                KfxSymbol::SectionPositionIdMap,
                rec.section_name.clone(),
                ion,
            )
        })
        .collect()
}

/// Navigation fragments for the PDF TOC, matching Amazon's PDF KFX shape: a
/// separate `nav_container` ($391) **entity** holding the table of contents, and
/// a thin `book_navigation` ($389) that merely *references* it by name. The
/// fixed-layout/PDOC reader requires this referenced form — an inline
/// nav_container (as the reflowable EPUB path emits, which that reader tolerates)
/// makes the device reject the whole book ("An error occurred…"). Returns an
/// empty vec when there's no usable outline.
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

/// Flat `page_list` entries — one per page, `{representation:{label},
/// target_position:{id, offset}}` (same shape as a TOC entry, no nesting). The
/// label is the PDF's page label (`pdf.page_labels`); the target is the page's
/// image EID, which `position_id_map` registers (an unregistered target would
/// make the device reject the book — see `build_pdf_toc_entries`).
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
/// `{representation:{label}, target_position:{id, offset}, [entries]}` structs
/// (no `nav_unit::` annotation, matching Amazon's PDF TOC). `target_position.id`
/// is the page's **image EID**, which is what `position_id_map` registers as a
/// reading position — the device resolves every nav target through that map, so
/// targeting an unregistered EID (e.g. the page container) makes it reject the
/// whole book. An item whose page is out of range is skipped along with its
/// subtree.
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
/// section → external_resource → shared bcRawMedia. The PDF variant differs
/// from the EPUB one in that every page's external_resource depends on the
/// *single* shared raw-media location, not a per-resource `resource/<name>`.
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
        return KfxFragment::singleton(KfxSymbol::ContainerEntityMap, ion);
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
    KfxFragment::singleton(KfxSymbol::ContainerEntityMap, ion)
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

    /// The synthesized cover TOC entry targets the cover **section root** — the
    /// same id the `cover_page` landmark uses. With the section-keyed position
    /// architecture the root is navigable, so the device merges TOC + landmark
    /// into one jumpable 表紙 (Amazon's shape). Pointing at a content eid instead
    /// gets the entry dropped on-device.
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

    #[test]
    fn test_content_features_fragment() {
        let ctx = ExportContext::new();
        let frag = build_content_features_fragment(&ctx);

        // Should be $585 (content_features) type
        assert_eq!(frag.ftype, KfxSymbol::ContentFeatures as u64);
        assert!(frag.is_singleton());

        // Extract Ion and verify structure
        if let crate::formats::kfx::fragment::FragmentData::Ion(ion) = &frag.data {
            if let IonValue::Struct(fields) = ion {
                // Should have features field
                let features = fields
                    .iter()
                    .find(|(id, _)| *id == KfxSymbol::Features as u64);
                assert!(
                    features.is_some(),
                    "content_features should contain features"
                );

                // Features should be a list with 3 items
                if let Some((_, IonValue::List(items))) = features {
                    assert_eq!(items.len(), 3, "should have 3 feature entries");
                } else {
                    panic!("features should be a list");
                }
            } else {
                panic!("expected Struct");
            }
        } else {
            panic!("expected Ion data");
        }
    }

    #[test]
    fn pick_document_writing_mode_any_vertical_beats_horizontal_majority() {
        // The LV999 case: 63 vertical-rl styles vs 91 horizontal (from
        // fixed-layout image pages). Vertical must win so the device turns
        // pages right-to-left.
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
        // Vertical-rl rtl book: the writing-mode `-rl` override does the work,
        // so direction stays ltr (matches Amazon).
        assert_eq!(
            direction_for_progression(true, KfxSymbol::VerticalRl),
            KfxSymbol::Ltr
        );
        // Horizontal rtl book (rtl manga): no `-rl` writing mode, so direction
        // must carry the turn.
        assert_eq!(
            direction_for_progression(true, KfxSymbol::HorizontalTb),
            KfxSymbol::Rtl
        );
        // Vertical-lr rtl (contradictory/Mongolian) still needs direction rtl.
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
                // max_id should be at least 100 (the IDs we generated)
                // Context starts at 866, so after 100 IDs we should be at 965
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
        let frag = build_content_features_fragment(&ctx);
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

        fragments.push(build_content_features_fragment(&ctx));
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

        // Verify entity type order matches reference pattern:
        // content_features, book_metadata, metadata, document_data, book_navigation,
        // sections (grouped), storylines (grouped), content (grouped)

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
        // Just verify that after storylines, we only have content entities (if any)
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

// NOTE: the former `section_type_tests` asserted that the titlepage section gets
// type:text (not type:container) when a *standalone* cover section also exists —
// which requires a book whose cover image differs from its titlepage image.
// epictetus had that (cover.jpg ≠ titlepage.png); the 人間失格 fixture's titlepage
// *is* the cover (cover.jpeg), so that branch can't be exercised here. Dropped
// with the epictetus fixture; re-add with a synthetic cover≠titlepage book if
// this path regresses.

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
        // The pipeline runs survey → chapters → images → finalize. (A coverless
        // book could skip "images"; the fixture has a cover, so all four fire.)
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
        // Phase 5: EPUB→KFX re-encodes interior raster plates as grayscale JXR
        // but keeps the COVER as JPEG — matching Amazon's own KFX (its pristine
        // download is a grayscale-JPEG cover + JXR plates) and what the Kindle
        // library-gallery / sleep-screen thumbnailer can read.
        let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let kfx = build_kfx_container(&mut book, &|_, _, _, _| {}).unwrap();
        let loaded = crate::kfx_to_epub::loader::load(&kfx).expect("load own kfx");
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
        // A genuinely-colorful plate (distinct R/G/B so it isn't auto-grayed).
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
        // 32×32 RGBA PNG: an opaque black 8×8 square top-left, everything
        // else fully-transparent black — the Feynman frontispiece shape.
        // Without alpha flattening, `to_luma8` maps the transparent region
        // to luma 0 and the plate renders as a black slab.
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
        // Full anchor-resolution flow on [太宰 治] 人間失格.epub, whose TOC links
        // point into the body chapters.
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
        // Test that anchor symbols are consistent between link_to and anchor creation
        // This tests the core invariant of the anchor registry
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

        // If we get here, no internal links were found (shouldn't happen with [太宰 治] 人間失格.epub)
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

    /// The cover is a solo unit; the rest pair into consecutive spreads with an
    /// odd tail page standing alone. Every page index appears exactly once, in
    /// order — the RTL facing order the device reads.
    #[test]
    fn page_groups_cover_solo_then_pairs_with_odd_tail() {
        assert!(manga_page_groups(0).is_empty());
        assert_eq!(manga_page_groups(1), vec![vec![0]]);
        assert_eq!(manga_page_groups(2), vec![vec![0], vec![1]]);
        assert_eq!(manga_page_groups(3), vec![vec![0], vec![1, 2]]);
        assert_eq!(manga_page_groups(4), vec![vec![0], vec![1, 2], vec![3]]);
        assert_eq!(manga_page_groups(5), vec![vec![0], vec![1, 2], vec![3, 4]]);
        let flat: Vec<usize> = manga_page_groups(11).into_iter().flatten().collect();
        assert_eq!(flat, (0..11).collect::<Vec<_>>());
    }

    fn unit(is_cover: bool, pages: &[(u64, u64, u64)]) -> MangaUnit {
        MangaUnit {
            section_name: "c0".into(),
            section_sym: 1,
            story_sym: 2,
            pt_id: 100,
            is_cover,
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

    /// A section's reading-position span drives both position maps and must
    /// match the EIDs actually laid down: a cover contributes its bare image
    /// (page_template + 1); a content page contributes outer→inner→image
    /// (page_template + 3 per page). Any drift dangles nav targets on-device.
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
    /// only when a real spread exists and `yj_thumbnails_present` only when a
    /// page carries a thumbnail (advertising an absent capability risks a device
    /// reject).
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
    /// axis + rtl direction for a manga (reference_kfx_device_rtl_writing_mode).
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
