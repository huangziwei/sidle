//! AZW3/KF8 format importer - handles all IO with lazy loading.
//!
//! AZW3 files use the KF8 (Kindle Format 8) structure with:
//! - Skeleton files for HTML structure
//! - Div elements for content fragments
//! - NCX index for table of contents

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::formats::mobi::parser::{
    DivElement, SkeletonFile, parse_div_index, parse_ncx_index, parse_skel_index, read_index,
};
use crate::formats::mobi::{
    Compression, Encoding, HuffCdicReader, MobiFormat, MobiHeader, NULL_INDEX, PdbInfo, TocNode,
    build_toc_from_ncx, detect_image_type, is_metadata_record, palmdoc, parse_exth, parse_fdst,
    strip_trailing_data, transform,
};
use crate::html::Stylesheet;
use crate::import::{ChapterId, Importer, SpineEntry, resolve_path_based_href};
use crate::io::{ByteSource, FileSource};
use crate::model::{AnchorTarget, Chapter, GlobalNodeId, Landmark, Metadata, TocEntry};

/// AZW3/KF8 format importer with lazy loading.
pub struct Azw3Importer {
    /// Random-access byte source.
    source: Arc<dyn ByteSource>,

    /// PDB header info.
    pdb: PdbInfo,

    /// MOBI header info.
    mobi: MobiHeader,

    /// Record offset for KF8 content (0 for pure KF8, >0 for combo files).
    record_offset: usize,

    /// PDB record index of the first image record. For pure KF8 this equals
    /// `mobi.first_image_index`; for KF8+MOBI6 combo files the images live
    /// in the MOBI6 section and the KF8 record0's `first_image_index` is
    /// past the image run, so this is saved from the MOBI6 record0 before
    /// the KF8 re-parse. `load_image_record` and `discover_assets` use this
    /// instead of recomputing from `mobi.first_image_index + record_offset`.
    image_record_base: usize,

    /// File length.
    file_len: u64,

    /// Book metadata.
    metadata: Metadata,

    /// Table of contents.
    toc: Vec<TocEntry>,

    /// Landmarks (structural navigation points).
    landmarks: Vec<Landmark>,

    /// Reading order (spine).
    spine: Vec<SpineEntry>,

    /// Chapter paths (filenames).
    chapter_paths: Vec<String>,

    /// KF8 structure for chapter reconstruction.
    kf8: Kf8Structure,

    /// Cached decompressed text (loaded on first chapter request).
    text_cache: Option<Vec<u8>>,

    /// `aid` attribute values that are link targets (some `kindle:pos:fid`
    /// link or NCX position resolves to them). Computed once from the full
    /// text; `build_chapter` keeps these as `id="aid-{value}"` instead of
    /// stripping them, so the resolved `#aid-…` hrefs land somewhere.
    linked_aids: Option<HashSet<String>>,

    /// Cached chapter content.
    chapter_cache: HashMap<u32, Vec<u8>>,

    /// Discovered asset paths.
    assets: Vec<PathBuf>,

    /// Cached parsed stylesheets.
    css_cache: HashMap<String, Stylesheet>,

    // --- Link resolution ---
    /// Maps "path#id" -> GlobalNodeId (built during index_anchors)
    element_id_map: HashMap<String, GlobalNodeId>,

    // --- TOC resolution ---
    /// NCX positions for TOC entries, keyed by (title, chapter_path).
    toc_positions: HashMap<(String, String), TocPosition>,
}

/// Position metadata for a TOC entry (from NCX).
#[derive(Debug, Clone, Copy)]
struct TocPosition {
    /// Byte position in the text stream.
    byte_pos: u32,
    /// File number (skeleton file).
    file_num: u32,
}

/// KF8 structure info parsed from indices.
struct Kf8Structure {
    /// Flow table from FDST (byte ranges in decompressed text).
    flow_table: Vec<(usize, usize)>,
    /// Skeleton files (chapter structure).
    files: Vec<SkeletonFile>,
    /// Div elements (content fragments).
    elems: Vec<DivElement>,
}

impl Importer for Azw3Importer {
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

    fn landmarks(&self) -> &[Landmark] {
        &self.landmarks
    }

    fn spine(&self) -> &[SpineEntry] {
        &self.spine
    }

    fn source_id(&self, id: ChapterId) -> Option<&str> {
        self.chapter_paths.get(id.0 as usize).map(|s| s.as_str())
    }

    fn load_raw(&mut self, id: ChapterId) -> io::Result<Vec<u8>> {
        // Check chapter cache first
        if let Some(content) = self.chapter_cache.get(&id.0) {
            return Ok(content.clone());
        }

        // Ensure text is loaded
        if self.text_cache.is_none() {
            self.text_cache = Some(self.extract_text()?);
        }
        self.ensure_linked_aids();

        // Build the requested chapter
        let text = self.text_cache.as_ref().unwrap();
        let linked_aids = self.linked_aids.as_ref().unwrap();
        let content = self.build_chapter(id.0, text, linked_aids)?;

        self.chapter_cache.insert(id.0, content.clone());
        Ok(content)
    }

    fn list_assets(&self) -> &[PathBuf] {
        &self.assets
    }

    fn load_asset(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        let key = path.to_string_lossy().replace('\\', "/");

        // Stylesheet: styles/styleNNNN.css → bytes from flow_table[idx+1].
        // KF8 packs CSS in flows 1..N (flow 0 is the HTML body); the
        // `kindle:flow:N` → `styles/style{N-1}.css` rewrite happens in
        // `transform_kindle_refs`, so the source HTML references stylesheets
        // by this naming. Serving the source bytes verbatim here is what
        // makes those links resolve in the emitted EPUB.
        if let Some(stem) = key
            .strip_prefix("styles/style")
            .and_then(|s| s.strip_suffix(".css"))
        {
            let css_idx: usize = stem.parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Invalid CSS asset path: {}", key),
                )
            })?;
            let flow_idx = css_idx + 1;
            if self.text_cache.is_none() {
                self.text_cache = Some(self.extract_text()?);
            }
            let text = self.text_cache.as_ref().unwrap();
            let (start, end) = self.kf8.flow_table.get(flow_idx).copied().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Flow {} not present in flow_table", flow_idx),
                )
            })?;
            let end = end.min(text.len());
            // Native Amazon stylesheets often chain-load each other via
            // `@import url(kindle:flow:0001?mime=text/css);`. Rewriting those
            // URLs to sibling-relative `styleNNNN.css` paths lets Apple Books
            // resolve the import chain (otherwise the writing-mode / class
            // rules in the imported sheet never load). Calibre-converted
            // AZW3s don't carry such imports — the pass is a no-op for them.
            // Embedded-font `@font-face` rules are dropped: their
            // `kindle:embed:` sources point at FONT records the EPUB doesn't
            // ship, so the rule can only ever dangle.
            let css = transform::rewrite_kindle_flow_in_css(&text[start..end]);
            return Ok(transform::strip_kindle_embed_font_faces(&css));
        }

        // Image: images/image_NNNN.ext
        let idx: usize = key
            .strip_prefix("images/image_")
            .and_then(|s| s.split('.').next())
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Invalid asset path: {}", key),
                )
            })?;

        self.load_image_record(idx)
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
        Some(sheet)
    }

    fn index_anchors(&mut self, chapters: &[(ChapterId, Arc<Chapter>)]) {
        self.element_id_map.clear();

        // Build path#id → GlobalNodeId map from chapters (same format as EPUB)
        for (chapter_id, chapter) in chapters {
            // Get the chapter's source path
            let chapter_path = match self.chapter_paths.get(chapter_id.0 as usize) {
                Some(p) => p.as_str(),
                None => continue,
            };

            for node_id in chapter.iter_dfs() {
                if let Some(id) = chapter.semantics.id(node_id) {
                    let key = format!("{}#{}", chapter_path, id);
                    self.element_id_map
                        .insert(key, GlobalNodeId::new(*chapter_id, node_id));
                }
            }
        }
    }

    fn resolve_href(&self, from_chapter: ChapterId, href: &str) -> Option<AnchorTarget> {
        self.resolve_href_impl(from_chapter, href, false)
    }

    fn resolve_toc_href(&self, from_chapter: ChapterId, href: &str) -> Option<AnchorTarget> {
        self.resolve_href_impl(from_chapter, href, true)
    }

    fn resolve_toc(&mut self) {
        // Load text if not cached
        if self.text_cache.is_none() {
            if let Ok(text) = self.extract_text() {
                self.text_cache = Some(text);
            } else {
                return;
            }
        }

        let text = self.text_cache.as_ref().unwrap();

        // Get HTML flow (flow 0)
        let (html_start, html_end) = self
            .kf8
            .flow_table
            .first()
            .copied()
            .unwrap_or((0, text.len()));
        let html_text = &text[html_start..html_end.min(text.len())];

        // Build file_starts for find_nearest_id_fast
        let file_starts: Vec<(u32, u32)> = self
            .kf8
            .files
            .iter()
            .map(|f| (f.start_pos, f.file_number as u32))
            .collect();

        // Resolve TOC entries using stored positions
        resolve_toc_with_positions(&mut self.toc, &self.toc_positions, html_text, &file_starts);
    }
}

/// Recursively resolve TOC entry hrefs with fragment IDs using position map.
fn resolve_toc_with_positions(
    entries: &mut [TocEntry],
    positions: &HashMap<(String, String), TocPosition>,
    html_text: &[u8],
    file_starts: &[(u32, u32)],
) {
    for entry in entries {
        // Look up position by (title, chapter_path)
        let chapter_path = entry.href.split('#').next().unwrap_or(&entry.href);
        let key = (entry.title.clone(), chapter_path.to_string());

        if let Some(pos) = positions.get(&key) {
            // Find nearest ID at this position
            if let Some(id) = transform::find_nearest_id_fast(
                html_text,
                pos.byte_pos as usize,
                pos.file_num as usize,
                file_starts,
            ) {
                // Update href with fragment
                if !entry.href.contains('#') {
                    entry.href = format!("{}#{}", entry.href, id);
                }
            }
        }

        // Recurse into children
        resolve_toc_with_positions(&mut entry.children, positions, html_text, file_starts);
    }
}

impl Azw3Importer {
    /// Shared href resolver; `chapter_fallback` lands a dead `path#fragment` at
    /// the chapter start (navigation) instead of returning `None` (in-text).
    fn resolve_href_impl(
        &self,
        from_chapter: ChapterId,
        href: &str,
        chapter_fallback: bool,
    ) -> Option<AnchorTarget> {
        let from_path = self.source_id(from_chapter)?;
        resolve_path_based_href(
            from_path,
            href,
            |p| {
                self.chapter_paths
                    .iter()
                    .position(|cp| cp == p)
                    .map(|i| ChapterId(i as u32))
            },
            |k| self.element_id_map.get(k).copied(),
            chapter_fallback,
        )
    }

    /// Create an importer from a ByteSource (metadata only, text deferred).
    pub fn from_source(source: Arc<dyn ByteSource>) -> io::Result<Self> {
        let file_len = source.len();

        // Read PDB header
        let header_start = source.read_at(0, 78)?;
        if header_start.len() < 78 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "File too short for PDB header",
            ));
        }

        let num_records = u16::from_be_bytes([header_start[76], header_start[77]]) as usize;
        let header_size = 78 + num_records * 8;
        let header_bytes = source.read_at(0, header_size)?;
        let (pdb, _) = PdbInfo::parse(&header_bytes)?;

        if pdb.num_records < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Not enough records",
            ));
        }

        // Helper to read a record
        let read_record = |idx: usize| -> io::Result<Vec<u8>> {
            let (start, end) = pdb.record_range(idx, file_len)?;
            source.read_at(start, (end - start) as usize)
        };

        // Parse record 0 (MOBI header)
        let record0 = read_record(0)?;
        let mobi = MobiHeader::parse(&record0)?;

        if mobi.encryption != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Encrypted files are not supported",
            ));
        }

        // Parse EXTH metadata
        let exth = parse_exth(&record0, &mobi);

        // Detect format and get record offset
        let format = detect_format(&mobi, &exth, &pdb, &read_record)?;
        let record_offset = format.record_offset();

        // For combo files the MOBI6 record0 carries the actual image-record
        // base — kindlegen 2.x leaves images in the MOBI6 section and the
        // KF8 record0's `first_image_index` field is past the image run.
        // Capture MOBI6's value before the `mobi` variable gets reassigned
        // to the KF8 header below.
        let mobi6_first_image_index = mobi.first_image_index as usize;

        // For combo files, re-parse KF8 header
        let mobi = if record_offset > 0 {
            let kf8_record0 = read_record(record_offset)?;
            MobiHeader::parse(&kf8_record0)?
        } else {
            mobi
        };

        // Pure KF8 → images live alongside the KF8 records, indexed off
        // `mobi.first_image_index`. Combo → images live in MOBI6 records,
        // indexed off the MOBI6-record0 value captured above.
        let image_record_base = if record_offset > 0 {
            mobi6_first_image_index
        } else {
            mobi.first_image_index as usize
        };

        // Verify this is KF8
        if !format.is_kf8() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Not a KF8/AZW3 file - use MobiImporter for MOBI6 files",
            ));
        }

        // Build metadata
        let mut metadata = build_metadata(&pdb, &mobi, &exth);

        // Parse KF8 indices (without reading text content)
        let codec = match mobi.encoding {
            Encoding::Utf8 => "utf-8",
            _ => "cp1252",
        };

        let mut read_record_offset = |idx: usize| -> io::Result<Vec<u8>> {
            let actual_idx = idx + record_offset;
            let (start, end) = pdb.record_range(actual_idx, file_len)?;
            source.read_at(start, (end - start) as usize)
        };

        // Parse FDST
        let flow_table = if mobi.fdst_index != NULL_INDEX {
            let fdst_record = read_record_offset(mobi.fdst_index as usize)?;
            parse_fdst(&fdst_record)?
        } else {
            Vec::new()
        };

        // Parse skeleton index
        let files = if mobi.skel_index != NULL_INDEX {
            let (entries, _) =
                read_index(&mut read_record_offset, mobi.skel_index as usize, codec)?;
            parse_skel_index(&entries)
        } else {
            Vec::new()
        };

        // Parse div index
        let elems = if mobi.div_index != NULL_INDEX {
            let (entries, cncx) =
                read_index(&mut read_record_offset, mobi.div_index as usize, codec)?;
            parse_div_index(&entries, &cncx)
        } else {
            Vec::new()
        };

        // Parse NCX for TOC
        let ncx = if mobi.ncx_index != NULL_INDEX {
            let (entries, cncx) =
                read_index(&mut read_record_offset, mobi.ncx_index as usize, codec)?;
            parse_ncx_index(&entries, &cncx)
        } else {
            Vec::new()
        };

        // Build spine from skeleton files
        let mut spine = Vec::new();
        let mut chapter_paths = Vec::new();
        for (i, file) in files.iter().enumerate() {
            let filename = format!("part{:04}.html", file.file_number);
            chapter_paths.push(filename);
            spine.push(SpineEntry {
                id: ChapterId(i as u32),
                size_estimate: file.length as usize,
                page_spread: None,
            });
        }

        // Build hierarchical TOC and collect positions for later resolution
        let mut toc_positions = HashMap::new();
        let toc = {
            let nodes = build_toc_from_ncx(&ncx, |entry| {
                // KF8 uses pos_fid (frag_idx, offset) - calculate actual byte position
                // frag_idx is index into fragment/div table, offset is added to insert_pos
                let (file_num, byte_pos) = if let Some((frag_idx, offset)) = entry.pos_fid
                    && let Some(elem) = elems.get(frag_idx as usize)
                {
                    // Position is elem's insert_pos + offset (like KindleUnpack)
                    (elem.file_number as usize, elem.insert_pos + offset)
                } else {
                    // Fall back to absolute position
                    let file_num = find_file_for_position(&files, entry.pos)
                        .map(|f| f.file_number)
                        .unwrap_or(0);
                    (file_num, entry.pos)
                };

                let chapter_path = format!("part{:04}.html", file_num);

                // Store position keyed by (title, chapter_path)
                // Use unescaped title to match TocEntry.title
                let title = quick_xml::escape::unescape(&entry.text)
                    .map(|s| s.into_owned())
                    .unwrap_or_else(|_| entry.text.clone());
                let key = (title, chapter_path.clone());
                toc_positions.insert(
                    key,
                    TocPosition {
                        byte_pos,
                        file_num: file_num as u32,
                    },
                );

                chapter_path
            });
            nodes.into_iter().map(toc_node_to_entry).collect()
        };

        // Find cover image
        if let Some(exth) = exth
            && let Some(cover_idx) = exth.cover_offset
        {
            metadata.cover_image = Some(format!("images/image_{:04}.jpg", cover_idx));
        }

        let mut importer = Self {
            source,
            pdb,
            mobi,
            record_offset,
            image_record_base,
            file_len,
            metadata,
            toc,
            landmarks: Vec::new(), // AZW3 format doesn't have landmarks
            spine,
            chapter_paths,
            kf8: Kf8Structure {
                flow_table,
                files,
                elems,
            },
            text_cache: None,
            linked_aids: None,
            chapter_cache: HashMap::new(),
            assets: Vec::new(),
            css_cache: HashMap::new(),
            element_id_map: HashMap::new(),
            toc_positions,
        };

        importer.assets = importer.discover_assets();
        // Filter out flows that are actually SVG illustration content —
        // those get inlined into chapter HTML by `inline_svg_flows` and
        // are dead weight (plus they leak `kindle:embed:` URLs) when
        // emitted as `.css` assets.
        importer.prune_svg_flow_assets();

        // Cover fallback: older Japanese-Amazon kindlegen output omits
        // EXTH 201 and only carries the cover ref in EXTH 129's KF8
        // `kindle:embed:NNNN` form, whose 1-based resource-index scheme
        // doesn't map cleanly to the image-only subset (decoded indices
        // overshoot by 1–2 vs the actual cover). Kindlegen always emits
        // the cover as the first image record, so when 201 is absent
        // fall back to the first `images/` asset.
        if importer.metadata.cover_image.is_none()
            && let Some(first_image) = importer.assets.iter().find(|p| p.starts_with("images"))
        {
            importer.metadata.cover_image = Some(first_image.to_string_lossy().into_owned());
        }

        Ok(importer)
    }

    /// Extract and decompress text content (called on first chapter request).
    fn extract_text(&self) -> io::Result<Vec<u8>> {
        let mut text = Vec::new();

        let read_record = |idx: usize| -> io::Result<Vec<u8>> {
            let actual_idx = idx + self.record_offset;
            let (start, end) = self.pdb.record_range(actual_idx, self.file_len)?;
            self.source.read_at(start, (end - start) as usize)
        };

        // Build decompressor if needed
        let mut huff_reader = if self.mobi.compression == Compression::Huffman
            && self.mobi.huff_record_index != NULL_INDEX
        {
            let huff_data = read_record(self.mobi.huff_record_index as usize)?;
            let mut cdics = Vec::new();
            for i in 0..self.mobi.huff_record_count.saturating_sub(1) {
                let cdic_idx = self.mobi.huff_record_index as usize + 1 + i as usize;
                if let Ok(cdic) = read_record(cdic_idx) {
                    cdics.push(cdic);
                }
            }
            let cdic_refs: Vec<&[u8]> = cdics.iter().map(|c| c.as_slice()).collect();
            Some(HuffCdicReader::new(&huff_data, &cdic_refs)?)
        } else {
            None
        };

        // Read and decompress text records
        for i in 1..=self.mobi.text_record_count as usize {
            let record = read_record(i)?;
            let stripped = strip_trailing_data(&record, self.mobi.extra_data_flags);

            let decompressed = match self.mobi.compression {
                Compression::None => stripped.to_vec(),
                Compression::PalmDoc => palmdoc::decompress(stripped)?,
                Compression::Huffman => {
                    if let Some(ref mut reader) = huff_reader {
                        reader.decompress(stripped)?
                    } else {
                        stripped.to_vec()
                    }
                }
                Compression::Unknown(_) => stripped.to_vec(),
            };

            text.extend_from_slice(&decompressed);
        }

        Ok(text)
    }

    /// Compute the link-target `aid` set once (see the `linked_aids` field).
    /// Requires `text_cache` to be populated; sets `Some` even on a missing
    /// cache (empty set) so callers can unwrap after a successful text load.
    fn ensure_linked_aids(&mut self) {
        if self.linked_aids.is_some() {
            return;
        }
        let Some(text) = self.text_cache.as_ref() else {
            self.linked_aids = Some(HashSet::new());
            return;
        };
        let (html_start, html_end) = self
            .kf8
            .flow_table
            .first()
            .copied()
            .unwrap_or((0, text.len()));
        let html_text = &text[html_start..html_end.min(text.len())];
        let file_starts: Vec<(u32, u32)> = self
            .kf8
            .files
            .iter()
            .map(|f| (f.start_pos, f.file_number as u32))
            .collect();
        // NCX TOC entries resolve through the same nearest-attribute lookup
        // (`resolve_toc`), so their positions are link sources too.
        let toc_targets: Vec<(usize, usize)> = self
            .toc_positions
            .values()
            .map(|p| (p.byte_pos as usize, p.file_num as usize))
            .collect();
        self.linked_aids = Some(transform::collect_linked_aids(
            text,
            html_text,
            &self.kf8.elems,
            &file_starts,
            &toc_targets,
        ));
    }

    /// Build a specific chapter from cached text.
    fn build_chapter(
        &self,
        chapter_id: u32,
        text: &[u8],
        linked_aids: &HashSet<String>,
    ) -> io::Result<Vec<u8>> {
        // Get HTML content (flow 0)
        let (html_start, html_end) = self
            .kf8
            .flow_table
            .first()
            .copied()
            .unwrap_or((0, text.len()));
        let html_text = &text[html_start..html_end.min(text.len())];

        // Build all parts and return the requested one
        let parts = build_parts(html_text, &self.kf8.files, &self.kf8.elems);

        let content = parts
            .get(chapter_id as usize)
            .map(|(_, content)| content.clone())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Chapter {} not found", chapter_id),
                )
            })?;

        // Inline SVG flow content where the body uses
        // `<img src="kindle:flow:NNNN..."/>` to reference a full-page
        // illustration wrapper. Must precede `transform_kindle_refs` so the
        // raster-image `kindle:embed:NNNN` refs inside the inlined SVG get
        // rewritten in the same pass below.
        let inlined = transform::inline_svg_flows(&content, &self.kf8.flow_table, text);

        // Transform kindle: references to standard EPUB-style paths
        // This converts kindle:pos:fid:XXXX:off:YYYY to partNNNN.html#id
        let file_starts: Vec<(u32, u32)> = self
            .kf8
            .files
            .iter()
            .map(|f| (f.start_pos, f.file_number as u32))
            .collect();

        let transformed =
            transform::transform_kindle_refs(&inlined, &self.kf8.elems, html_text, &file_starts);

        // kindlegen's in-book TOC "Cover" rows can link straight at the
        // cover image — an EPUB 3 violation (RSC-010). Keep the label only.
        let transformed = transform::unlink_image_anchors(&transformed);

        // Drop dangling `<link>`s that escape the package root (e.g. the
        // Aozora `../styles/aNNNNN_h.css` horizontal alternate stylesheet that
        // was never embedded as a flow). transform_kindle_refs only rewrites
        // `kindle:flow:` hrefs, so this verbatim `..` href would otherwise
        // survive and fail strict EPUB-3 validation on import.
        let delinked = transform::strip_root_escaping_links(&transformed);

        // Strip Amazon-specific attributes (aid, data-Amzn*) — except
        // link-target aids, which become `id="aid-{value}"` anchors.
        let cleaned = transform::strip_kindle_attributes_fast(&delinked, linked_aids);

        // Ensure the root `<html>` carries both `xml:lang` and `lang`.
        // Calibre's AZW3 exporter scrubs `xml:lang` and leaves only `lang=`,
        // dropping per-spine-doc xml:lang counts versus the publisher EPUB.
        // We pair them up; the fallback to `metadata.language` covers AZW3s
        // that lack any lang signal on `<html>`.
        let with_lang = transform::ensure_html_lang_dual(&cleaned, &self.metadata.language);

        Ok(with_lang)
    }

    /// Drop `styles/styleNNNN.css` asset entries that correspond to flows
    /// whose actual content is SVG (full-page illustration wrappers). These
    /// get inlined into chapter HTML by `inline_svg_flows`, so emitting them
    /// as orphan `.css` assets ships dead bytes — and leaks the SVG's
    /// `kindle:embed:` URLs into the EPUB zip. Mirrors the same prefix-sniff
    /// `transform::inline_svg_flows` uses.
    fn prune_svg_flow_assets(&mut self) {
        if self.text_cache.is_none() {
            self.text_cache = self.extract_text().ok();
        }
        let Some(text) = self.text_cache.as_ref() else {
            return;
        };
        self.assets.retain(|p| {
            let Some(stem) = p
                .to_string_lossy()
                .strip_prefix("styles/style")
                .and_then(|s| s.strip_suffix(".css"))
                .and_then(|s| s.parse::<usize>().ok())
            else {
                return true; // not a CSS asset — keep
            };
            let flow_idx = stem + 1;
            let Some(&(start, end)) = self.kf8.flow_table.get(flow_idx) else {
                return true;
            };
            let end = end.min(text.len());
            !looks_like_svg_flow(&text[start..end])
        });
    }

    /// Discover asset paths by scanning image records and the flow table.
    fn discover_assets(&self) -> Vec<PathBuf> {
        let mut assets = Vec::new();

        // Stylesheets from the flow table. Flow 0 is HTML body; flows 1..N
        // are auxiliary resources, by convention CSS (matches the
        // `kindle:flow:N` → `styles/style{N-1}.css` rewrite in
        // `transform_kindle_refs`). Registering them here makes
        // `book.list_assets()` enumerate stylesheets so `export_raw` emits
        // them to the EPUB zip alongside images.
        for css_idx in 0..self.kf8.flow_table.len().saturating_sub(1) {
            assets.push(PathBuf::from(format!("styles/style{:04}.css", css_idx)));
        }

        if self.mobi.first_image_index == NULL_INDEX {
            return assets;
        }

        let first_img = self.image_record_base;
        for i in first_img..self.pdb.num_records as usize {
            // Only read first 16 bytes to detect type (magic bytes)
            if let Ok((start, end)) = self.pdb.record_range(i, self.file_len) {
                let read_len = 16.min((end - start) as usize);
                let mut header = [0u8; 16];
                if self
                    .source
                    .read_at_into(start, &mut header[..read_len])
                    .is_ok()
                {
                    let header = &header[..read_len];
                    if is_metadata_record(header) {
                        continue;
                    }
                    if let Some(media_type) = detect_image_type(header) {
                        let ext = match media_type {
                            "image/jpeg" => "jpg",
                            "image/png" => "png",
                            "image/gif" => "gif",
                            _ => "bin",
                        };
                        let idx = i - first_img;
                        assets.push(PathBuf::from(format!("images/image_{idx:04}.{ext}")));
                    }
                }
            }
        }

        assets
    }

    /// Load an image record by index.
    fn load_image_record(&self, idx: usize) -> io::Result<Vec<u8>> {
        let record_idx = self.image_record_base + idx;
        self.read_record(record_idx)
    }

    /// Read a record by absolute index.
    fn read_record(&self, idx: usize) -> io::Result<Vec<u8>> {
        let (start, end) = self.pdb.record_range(idx, self.file_len)?;
        self.source.read_at(start, (end - start) as usize)
    }
}

// ============================================================================
// Shared helpers
// ============================================================================

fn detect_format(
    mobi: &MobiHeader,
    exth: &Option<crate::formats::mobi::ExthHeader>,
    pdb: &PdbInfo,
    read_record: &dyn Fn(usize) -> io::Result<Vec<u8>>,
) -> io::Result<MobiFormat> {
    if mobi.mobi_version == 8 {
        return Ok(MobiFormat::Kf8);
    }

    if let Some(kf8_idx) = exth.as_ref().and_then(|e| e.kf8_boundary) {
        let boundary_idx = kf8_idx as usize - 1;
        if boundary_idx > 0 && boundary_idx < pdb.num_records as usize {
            let boundary = read_record(boundary_idx)?;
            if boundary.starts_with(b"BOUNDARY") {
                return Ok(MobiFormat::Combo {
                    kf8_record_offset: kf8_idx as usize,
                });
            }
        }
    }

    Ok(MobiFormat::Mobi6)
}

/// Parse a KF8 `original-resolution` value (`"1444x2048"`) into `(w, h)`.
fn parse_resolution(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

fn build_metadata(
    pdb: &PdbInfo,
    mobi: &MobiHeader,
    exth: &Option<crate::formats::mobi::ExthHeader>,
) -> Metadata {
    let title = exth
        .as_ref()
        .and_then(|e| e.title.clone())
        .or_else(|| {
            if !mobi.title.is_empty() {
                Some(mobi.title.clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| pdb.name.clone());

    let mut metadata = Metadata {
        title,
        ..Default::default()
    };

    if let Some(exth) = exth {
        metadata.authors = exth.authors.clone();
        metadata.publisher = exth.publisher.clone();
        metadata.description = exth.description.clone();
        metadata.subjects = exth.subjects.clone();
        metadata.date = exth.pub_date.clone();
        metadata.rights = exth.rights.clone();
        metadata.language = exth.language.clone().unwrap_or_default();
        metadata.identifier = exth
            .isbn
            .clone()
            .or_else(|| exth.asin.clone())
            .or_else(|| exth.source.clone())
            .unwrap_or_default();
        // EXTH 113 nominally holds an ASIN, but calibre's AZW3 exporter
        // writes a freshly-minted UUID there. Only promote to
        // `metadata.asin` when the value actually looks like an Amazon
        // ASIN (10-char alphanumeric starting with B for ebooks).
        metadata.asin = exth.asin.as_ref().filter(|s| looks_like_asin(s)).cloned();
        // Writing-mode signals (EXTH 525 / 527). Both calibre-exported AZW3s
        // and native Amazon AZW3s carry these; no fallback to inline HTML
        // class needed. Calibre's `reader/headers.py:96-108` is the spec.
        metadata.primary_writing_mode = exth.primary_writing_mode.clone();
        metadata.page_progression_direction = exth
            .page_progression_direction
            .clone()
            // Calibre derives PPD from writing-mode when EXTH 527 is absent:
            // anything ending `-rl` is RTL pagination.
            .or_else(|| {
                exth.primary_writing_mode.as_deref().and_then(|pwm| {
                    if pwm.ends_with("-rl") {
                        Some("rtl".to_string())
                    } else if pwm.ends_with("-lr") {
                        Some("ltr".to_string())
                    } else {
                        None
                    }
                })
            });

        // KF8 fixed-layout (comic / picture book): any of the three FXL EXTH
        // records marks the book as pre-paginated so it round-trips as a
        // fixed-layout EPUB instead of being flattened to reflowable.
        let book_type = exth.book_type.clone().filter(|s| !s.is_empty());
        let is_comic = book_type.as_deref() == Some("comic");
        metadata.fixed_layout = exth.fixed_layout.as_deref() == Some("true")
            || book_type.is_some()
            || exth.original_resolution.is_some();
        metadata.book_type = book_type;
        metadata.default_viewport = exth
            .original_resolution
            .as_deref()
            .and_then(parse_resolution);
        // KF8 has no explicit `rendition:spread`; a comic implies facing-page
        // (landscape) spreads, which is how the Kindle renders `book-type:comic`.
        if metadata.fixed_layout && is_comic {
            metadata.rendition_spread = Some("landscape".to_string());
        }
    }

    metadata
}

/// Amazon ASIN format: exactly 10 ASCII alphanumeric characters, typically
/// starting with `B` for ebook listings. Used to disambiguate EXTH 113 from
/// the UUID calibre's AZW3 exporter occasionally writes into the same slot.
fn looks_like_asin(s: &str) -> bool {
    s.len() == 10 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// True when a flow's first kilobyte holds an SVG document (after any
/// `<?xml-stylesheet?>` PI calibre's `mobi8.py` also peers past). Mirrors
/// the sniff in `transform::inline_svg_flows`.
fn looks_like_svg_flow(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    bstr::ByteSlice::find(head, b"<svg").is_some()
}

/// Build chapter parts by combining skeletons with div content.
fn build_parts(
    text: &[u8],
    files: &[SkeletonFile],
    elems: &[DivElement],
) -> Vec<(String, Vec<u8>)> {
    let mut parts = Vec::new();
    let mut div_ptr = 0;

    for file in files {
        let skel_start = file.start_pos as usize;
        let skel_end = skel_start + file.length as usize;

        if skel_end > text.len() {
            continue;
        }

        let mut skeleton = text[skel_start..skel_end].to_vec();
        let mut baseptr = skel_end;

        for _i in 0..file.div_count {
            if div_ptr >= elems.len() {
                break;
            }

            let elem = &elems[div_ptr];
            let part_len = elem.length as usize;

            if baseptr + part_len > text.len() {
                div_ptr += 1;
                continue;
            }

            let part = &text[baseptr..baseptr + part_len];
            let insert_pos = (elem.insert_pos as usize).saturating_sub(skel_start);

            if insert_pos <= skeleton.len() {
                let mut new_skeleton = Vec::with_capacity(skeleton.len() + part.len());
                new_skeleton.extend_from_slice(&skeleton[..insert_pos]);
                new_skeleton.extend_from_slice(part);
                new_skeleton.extend_from_slice(&skeleton[insert_pos..]);
                skeleton = new_skeleton;
            }

            baseptr += part_len;
            div_ptr += 1;
        }

        let filename = format!("part{:04}.html", file.file_number);
        parts.push((filename, skeleton));
    }

    if parts.is_empty() && !text.is_empty() {
        parts.push(("part0000.html".to_string(), text.to_vec()));
    }

    parts
}

fn find_file_for_position(files: &[SkeletonFile], pos: u32) -> Option<&SkeletonFile> {
    for file in files {
        if pos >= file.start_pos && pos < file.start_pos + file.length {
            return Some(file);
        }
    }
    files.first()
}

/// Convert TocNode to TocEntry recursively.
fn toc_node_to_entry(node: TocNode) -> TocEntry {
    let mut entry = TocEntry::new(&node.title, &node.href);
    entry.children = node.children.into_iter().map(toc_node_to_entry).collect();
    entry
}
