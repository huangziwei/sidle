//! MOBI6 format importer with chapter splitting.
//!
//! MOBI6 files are legacy Kindle format with a single HTML stream.
//! This importer splits the HTML at `<mbp:pagebreak>` boundaries to produce
//! multiple chapters, falling back to a single chapter if no pagebreaks exist.
//!
//! For KF8/AZW3 files, use Azw3Importer instead.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::formats::mobi::{
    Compression, Encoding, HuffCdicReader, MobiHeader, NULL_INDEX, PdbInfo, TocNode,
    asset_record_offset, build_toc_from_ncx, detect_image_type, filepos, is_metadata_record,
    palmdoc, parse_exth, parse_ncx_index, read_index, strip_trailing_data,
};
use crate::html::Stylesheet;
use crate::import::{ChapterId, Importer, SpineEntry, resolve_path_based_href};
use crate::io::{ByteSource, FileSource};
use crate::model::{AnchorTarget, Chapter, GlobalNodeId, Landmark, Metadata, TocEntry};

/// MOBI6 format importer with chapter splitting.
///
/// Splits MOBI HTML at `<mbp:pagebreak>` boundaries. Falls back to a single
/// chapter if no pagebreaks are found.
pub struct MobiImporter {
    /// Random-access byte source.
    source: Arc<dyn ByteSource>,

    /// PDB header info.
    pdb: PdbInfo,

    /// MOBI header info.
    mobi: MobiHeader,

    /// File length.
    file_len: u64,

    /// Book metadata.
    metadata: Metadata,

    /// Table of contents.
    toc: Vec<TocEntry>,

    /// Landmarks (structural navigation points).
    landmarks: Vec<Landmark>,

    /// Reading order.
    spine: Vec<SpineEntry>,

    /// Split chapter content (complete XHTML documents).
    chapter_cache: Vec<Vec<u8>>,

    /// Chapter file paths ("chapter_0.xhtml", "chapter_1.xhtml", ...).
    chapter_paths: Vec<String>,

    /// Discovered asset paths.
    assets: Vec<PathBuf>,

    /// Cached parsed stylesheets.
    css_cache: HashMap<String, Stylesheet>,

    // --- Link resolution ---
    /// Maps "path#id" -> GlobalNodeId (built during index_anchors)
    element_id_map: HashMap<String, GlobalNodeId>,
}

impl Importer for MobiImporter {
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
        self.chapter_cache
            .get(id.0 as usize)
            .cloned()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Chapter {} not found", id.0),
                )
            })
    }

    fn list_assets(&self) -> &[PathBuf] {
        &self.assets
    }

    fn load_asset(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        let key = path.to_string_lossy();

        let idx = asset_record_offset(path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Invalid asset path: {}", key),
            )
        })?;

        self.load_image_record(idx as usize)
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

        for (chapter_id, chapter) in chapters {
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
}

impl MobiImporter {
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

    /// Create an importer from a ByteSource.
    ///
    /// Text is extracted eagerly to determine chapter boundaries for the spine.
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

        // Read record 0 (MOBI header)
        let (start, end) = pdb.record_range(0, file_len)?;
        let record0 = source.read_at(start, (end - start) as usize)?;
        let mobi = MobiHeader::parse(&record0)?;

        if mobi.encryption != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Encrypted files are not supported",
            ));
        }

        // Parse EXTH metadata
        let exth = parse_exth(&record0, &mobi);

        // Build metadata
        let mut metadata = crate::formats::mobi::metadata::from_headers(&pdb, &mobi, &exth);

        // Discover assets to get cover image path with correct extension
        let assets = discover_assets_from_source(&source, &pdb, &mobi, file_len);

        // Find cover image using discovered asset path
        if let Some(ref exth) = exth
            && let Some(cover_idx) = exth.cover_offset
            && let Some(cover_path) = asset_at_record_offset(&assets, cover_idx)
        {
            metadata.cover_image = Some(cover_path.to_string_lossy().to_string());
        }

        // Parse NCX index BEFORE text transformation (needed for anchor insertion
        // and fallback split points)
        let codec = match mobi.encoding {
            Encoding::Utf8 => "utf-8",
            _ => "cp1252",
        };

        let ncx_entries = if mobi.ncx_index != NULL_INDEX {
            let mut read_record = |idx: usize| -> io::Result<Vec<u8>> {
                let (start, end) = pdb.record_range(idx, file_len)?;
                source.read_at(start, (end - start) as usize)
            };

            match read_index(&mut read_record, mobi.ncx_index as usize, codec) {
                Ok((entries, cncx)) => Some(parse_ncx_index(&entries, &cncx)),
                Err(_) => None,
            }
        } else {
            None
        };

        // Extract NCX positions for anchor insertion and fallback splitting
        let ncx_positions: Vec<u32> = ncx_entries
            .as_ref()
            .map(|entries| entries.iter().map(|e| e.pos).collect())
            .unwrap_or_default();

        // Extract and transform text eagerly (needed to determine chapter count)
        let text = extract_text_from_source(&source, &pdb, &mobi, file_len)?;
        let wrapped = wrap_text_as_html(&text, &metadata.title, &mobi);

        // Transform HTML with an anchor at every NCX target. The anchors are
        // inert `<a id="fileposN" />` markers, but they are the only thing
        // `collect_filepos_anchors` can read to learn which chapter each NCX
        // target landed in. A source whose NCX targets are also `filepos=`
        // link targets gets them either way; one that navigates purely through
        // the index (every periodical does) has no `filepos=` at all, and
        // without these every TOC entry resolves to chapter 0.
        let transformed = filepos::transform_mobi_html(&wrapped, &assets, &ncx_positions);

        // Pagebreaks are the primary split. NCX positions are the fallback for
        // a source that carries none — the anchors are already in place, so
        // this only has to choose which boundaries to cut on.
        let split = {
            let initial = split_mobi_html(&transformed, None, &metadata.title);
            if initial.chapters.len() > 1 || ncx_positions.is_empty() {
                initial
            } else {
                let ncx_split =
                    split_mobi_html_ncx_only(&transformed, &ncx_positions, &metadata.title);
                if ncx_split.chapters.len() > 1 {
                    ncx_split
                } else {
                    initial
                }
            }
        };

        let mut split = split;

        // An index entry's link target, resolved through the chapter the
        // anchor landed in.
        let href_for = |entry: &crate::formats::mobi::index::NcxEntry| {
            let chapter_idx = split
                .filepos_to_chapter
                .get(&format!("filepos{}", entry.pos))
                .copied()
                .unwrap_or(0);
            format!("{}#filepos{}", split.chapter_paths[chapter_idx], entry.pos)
        };

        // Build TOC from NCX entries (using split result for chapter mapping)
        let mut toc = if let Some(ref ncx) = ncx_entries {
            let nodes = build_toc_from_ncx(ncx, &href_for);
            nodes.into_iter().map(toc_node_to_entry).collect()
        } else {
            vec![TocEntry::new(&metadata.title, &split.chapter_paths[0])]
        };

        // A periodical carries no contents page of its own — the index is the
        // whole of its navigation, and only Amazon's periodical reader draws a
        // page from it. Render one, so the issue opens on what is in it.
        if metadata.periodical.is_some()
            && let Some(ref ncx) = ncx_entries
            && let Some(page) = crate::formats::mobi::periodical::issue_front_matter(
                ncx,
                crate::formats::mobi::metadata::publication_title(
                    &metadata.title,
                    metadata.date.as_deref(),
                ),
                metadata
                    .date
                    .as_deref()
                    .map(crate::util::truncate_to_date)
                    .as_deref(),
                href_for,
                |offset| {
                    asset_at_record_offset(&assets, offset)
                        .map(|p| p.to_string_lossy().into_owned())
                },
            )
        {
            let path = ISSUE_FRONT_MATTER_PATH.to_string();
            // The index's own root entry means "the contents of this issue",
            // so it is the natural link to the page rather than a second entry
            // saying the same thing.
            if let Some(root) = toc.first_mut()
                && ncx.first().is_some_and(|e| {
                    e.kind
                        .as_deref()
                        .is_some_and(|k| k.eq_ignore_ascii_case("periodical"))
                })
            {
                root.href = path.clone();
            }
            split
                .chapters
                .insert(0, wrap_front_matter(&page, &metadata.title));
            split.chapter_paths.insert(0, path);
        }

        // Build spine from split chapters
        let spine: Vec<SpineEntry> = (0..split.chapters.len())
            .map(|i| SpineEntry {
                id: ChapterId(i as u32),
                size_estimate: split.chapters[i].len(),
                page_spread: None,
                viewport: None,
                panels: Vec::new(),
            })
            .collect();

        let mut importer = Self {
            source,
            pdb,
            mobi,
            file_len,
            metadata,
            toc,
            landmarks: Vec::new(),
            spine,
            chapter_cache: split.chapters,
            chapter_paths: split.chapter_paths,
            assets: Vec::new(),
            css_cache: HashMap::new(),
            element_id_map: HashMap::new(),
        };

        importer.assets = importer.discover_assets();

        Ok(importer)
    }

    /// Discover asset paths by scanning image records.
    fn discover_assets(&self) -> Vec<PathBuf> {
        let mut assets = Vec::new();

        if self.mobi.first_image_index == NULL_INDEX {
            return assets;
        }

        let first_img = self.mobi.first_image_index as usize;
        for i in first_img..self.pdb.num_records as usize {
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
        let first_img = self.mobi.first_image_index as usize;
        let record_idx = first_img + idx;
        self.read_record(record_idx)
    }

    /// Read a record by index.
    fn read_record(&self, idx: usize) -> io::Result<Vec<u8>> {
        let (start, end) = self.pdb.record_range(idx, self.file_len)?;
        self.source.read_at(start, (end - start) as usize)
    }
}

// ============================================================================
// Text extraction (standalone, for use during from_source)
// ============================================================================

/// Extract and decompress text content from a MOBI source.
fn extract_text_from_source(
    source: &Arc<dyn ByteSource>,
    pdb: &PdbInfo,
    mobi: &MobiHeader,
    file_len: u64,
) -> io::Result<Vec<u8>> {
    let mut text = Vec::new();

    let read_record = |idx: usize| -> io::Result<Vec<u8>> {
        let (start, end) = pdb.record_range(idx, file_len)?;
        source.read_at(start, (end - start) as usize)
    };

    // Build decompressor if needed
    let mut huff_reader =
        if mobi.compression == Compression::Huffman && mobi.huff_record_index != NULL_INDEX {
            let huff_data = read_record(mobi.huff_record_index as usize)?;
            let mut cdics = Vec::new();
            for i in 0..mobi.huff_record_count.saturating_sub(1) {
                let cdic_idx = mobi.huff_record_index as usize + 1 + i as usize;
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
    for i in 1..=mobi.text_record_count as usize {
        let record = read_record(i)?;
        let stripped = strip_trailing_data(&record, mobi.extra_data_flags);

        let decompressed = match mobi.compression {
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

// ============================================================================
// Chapter splitting
// ============================================================================

/// Result of splitting MOBI HTML into chapters.
struct ChapterSplit {
    /// Split chapter content (complete XHTML documents).
    chapters: Vec<Vec<u8>>,
    /// Chapter file paths.
    chapter_paths: Vec<String>,
    /// Maps "fileposN" → chapter index.
    filepos_to_chapter: HashMap<String, usize>,
}

/// Split transformed MOBI HTML into chapters at `<mbp:pagebreak>` boundaries.
///
/// Falls back to NCX position-based splitting if no pagebreaks are found.
/// Falls back to a single chapter if neither pagebreaks nor NCX positions exist.
fn split_mobi_html(html: &[u8], ncx_positions: Option<&[u32]>, title: &str) -> ChapterSplit {
    let html_str = String::from_utf8_lossy(html);

    // Extract <head> content and <body> content
    let (head_content, body_content) = extract_head_and_body(&html_str);
    let head_content = sanitize_mobi_head(&head_content, title);

    // Find pagebreak positions in the body content, preferring the ones that
    // separate top-level siblings so no element is cut in half.
    let all_pagebreaks = find_pagebreaks(body_content.as_bytes());
    let pagebreak_positions =
        pagebreaks_at_top_level(body_content.as_bytes(), &all_pagebreaks).unwrap_or(all_pagebreaks);

    // Split body: pagebreaks first, NCX fallback, then single chapter
    let body_chunks = if !pagebreak_positions.is_empty() {
        split_at_pagebreaks(&body_content, &pagebreak_positions)
    } else if let Some(positions) = ncx_positions {
        let ncx_chunks = split_at_ncx_anchors(&body_content, positions);
        if ncx_chunks.len() > 1 {
            ncx_chunks
        } else {
            vec![body_content.to_string()]
        }
    } else {
        vec![body_content.to_string()]
    };

    // Rewrite any pagebreak the split did not consume into a page-break div
    // (`mbp:` is unbound in an XHTML chapter), then fold away the chunks that
    // hold nothing a reader could see.
    let body_chunks = coalesce_contentless_chunks(
        body_chunks
            .into_iter()
            .map(|chunk| replace_leftover_pagebreaks(&chunk))
            .collect(),
    );

    // Build chapter documents and filepos map
    let mut chapters = Vec::with_capacity(body_chunks.len());
    let mut chapter_paths = Vec::with_capacity(body_chunks.len());
    let mut filepos_to_chapter: HashMap<String, usize> = HashMap::new();

    for (i, chunk) in body_chunks.iter().enumerate() {
        let chapter_path = format!("chapter_{}.xhtml", i);
        chapter_paths.push(chapter_path);

        // Scan this chunk for filepos anchors and record their chapter
        collect_filepos_anchors(chunk, i, &mut filepos_to_chapter);

        // Wrap chunk as complete XHTML
        let doc = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <!DOCTYPE html>\n\
             <html xmlns=\"http://www.w3.org/1999/xhtml\">\n\
             <head>\n{}</head>\n\
             <body>\n{}\n</body>\n\
             </html>",
            head_content, chunk
        );
        chapters.push(doc.into_bytes());
    }

    // Rewrite cross-chapter links
    rewrite_cross_chapter_links(&mut chapters, &filepos_to_chapter, &chapter_paths);

    // Neutralize bare filename links (OEB source references that don't exist in EPUB)
    neutralize_bare_filename_links(&mut chapters);

    // Ensure at least one chapter
    if chapters.is_empty() {
        chapters.push(html.to_vec());
        chapter_paths.push("chapter_0.xhtml".to_string());
    }

    ChapterSplit {
        chapters,
        chapter_paths,
        filepos_to_chapter,
    }
}

/// Split MOBI HTML using only NCX positions, bypassing pagebreak detection.
///
/// Used when pagebreak-based splitting fails to produce multiple chapters
/// but NCX index entries provide valid split points.
fn split_mobi_html_ncx_only(html: &[u8], ncx_positions: &[u32], title: &str) -> ChapterSplit {
    let html_str = String::from_utf8_lossy(html);
    let (head_content, body_content) = extract_head_and_body(&html_str);
    let head_content = sanitize_mobi_head(&head_content, title);

    let body_chunks = {
        let ncx_chunks = split_at_ncx_anchors(&body_content, ncx_positions);
        if ncx_chunks.len() > 1 {
            ncx_chunks
        } else {
            vec![body_content.to_string()]
        }
    };

    // Rewrite any pagebreak the split did not consume into a page-break div
    // (`mbp:` is unbound in an XHTML chapter), then fold away the chunks that
    // hold nothing a reader could see.
    let body_chunks = coalesce_contentless_chunks(
        body_chunks
            .into_iter()
            .map(|chunk| replace_leftover_pagebreaks(&chunk))
            .collect(),
    );

    // Build chapter documents and filepos map
    let mut chapters = Vec::with_capacity(body_chunks.len());
    let mut chapter_paths = Vec::with_capacity(body_chunks.len());
    let mut filepos_to_chapter: HashMap<String, usize> = HashMap::new();

    for (i, chunk) in body_chunks.iter().enumerate() {
        let chapter_path = format!("chapter_{}.xhtml", i);
        chapter_paths.push(chapter_path);
        collect_filepos_anchors(chunk, i, &mut filepos_to_chapter);

        let doc = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <!DOCTYPE html>\n\
             <html xmlns=\"http://www.w3.org/1999/xhtml\">\n\
             <head>\n{}</head>\n\
             <body>\n{}\n</body>\n\
             </html>",
            head_content, chunk
        );
        chapters.push(doc.into_bytes());
    }

    rewrite_cross_chapter_links(&mut chapters, &filepos_to_chapter, &chapter_paths);
    neutralize_bare_filename_links(&mut chapters);

    if chapters.is_empty() {
        chapters.push(html.to_vec());
        chapter_paths.push("chapter_0.xhtml".to_string());
    }

    ChapterSplit {
        chapters,
        chapter_paths,
        filepos_to_chapter,
    }
}

/// Make a MOBI source head usable as an XHTML chapter head: drop the
/// MOBI-only `<guide>…</guide>` block (an OPF concept — invalid in XHTML and
/// already consumed for navigation elsewhere) and ensure a `<title>` child
/// (required by the XHTML content model; RSC-017 without it).
fn sanitize_mobi_head(head: &str, title: &str) -> String {
    let mut out = String::with_capacity(head.len());
    let lower = head.to_ascii_lowercase();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("<guide") {
        let start = pos + rel;
        out.push_str(&head[pos..start]);
        match lower[start..].find("</guide>") {
            Some(end_rel) => pos = start + end_rel + "</guide>".len(),
            None => {
                pos = head.len();
                break;
            }
        }
    }
    out.push_str(&head[pos..]);
    if !out.to_ascii_lowercase().contains("<title") {
        out.push_str(&format!(
            "<title>{}</title>\n",
            title
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        ));
    }
    out
}

/// Extract the content inside `<head>...</head>` and `<body>...</body>`.
///
/// Returns (head_inner, body_inner). If tags aren't found, returns reasonable
/// defaults.
fn extract_head_and_body(html: &str) -> (String, String) {
    let html_lower = html.to_ascii_lowercase();

    // Find <head> content
    let head_content = if let Some(head_start) = html_lower.find("<head") {
        let after_tag = html[head_start..].find('>').map(|p| head_start + p + 1);
        let head_end = html_lower.find("</head>");
        match (after_tag, head_end) {
            (Some(start), Some(end)) if start <= end => html[start..end].to_string(),
            _ => String::new(),
        }
    } else {
        String::new()
    };

    // Find <body> content
    let body_content = if let Some(body_start) = html_lower.find("<body") {
        let after_tag = html[body_start..].find('>').map(|p| body_start + p + 1);
        let body_end = html_lower.rfind("</body>");
        match (after_tag, body_end) {
            (Some(start), Some(end)) if start <= end => html[start..end].to_string(),
            (Some(start), None) => html[start..].to_string(),
            _ => html.to_string(),
        }
    } else {
        html.to_string()
    };

    (head_content, body_content)
}

/// A pagebreak location: byte range of the `<mbp:pagebreak...>` tag in the body.
struct PagebreakPos {
    /// Start byte offset of the `<` character.
    start: usize,
    /// End byte offset (one past the `>` character).
    end: usize,
}

/// Find all `<mbp:pagebreak...>` tags in body content.
///
/// Matches variants: `<mbp:pagebreak/>`, `<mbp:pagebreak />`,
/// `<mbp:pagebreak>`, with optional attributes, case-insensitive.
fn find_pagebreaks(body: &[u8]) -> Vec<PagebreakPos> {
    let mut results = Vec::new();
    let body_lower: Vec<u8> = body.iter().map(|b| b.to_ascii_lowercase()).collect();
    let needle = b"<mbp:pagebreak";

    let mut pos = 0;
    while pos + needle.len() < body_lower.len() {
        if let Some(rel) = body_lower[pos..]
            .windows(needle.len())
            .position(|w| w == needle)
        {
            let tag_start = pos + rel;
            // Find the closing > for this tag
            if let Some(close_rel) = body[tag_start..].iter().position(|&b| b == b'>') {
                let tag_end = tag_start + close_rel + 1;
                results.push(PagebreakPos {
                    start: tag_start,
                    end: tag_end,
                });
                pos = tag_end;
            } else {
                pos = tag_start + needle.len();
            }
        } else {
            break;
        }
    }

    results
}

/// HTML elements that never have content, so a closing tag for them opens
/// nothing and closes nothing.
///
/// MOBI6 writes several of these as pairs — `<br> </br>`, `<img …> </IMG>` —
/// which is why both halves have to be ignored rather than just the open tag:
/// counting `</br>` as a close would drive the element depth negative and make
/// every later boundary look wrong.
const VOID_ELEMENTS: &[&[u8]] = &[
    b"area",
    b"base",
    b"br",
    b"col",
    b"embed",
    b"hr",
    b"img",
    b"input",
    b"link",
    b"mbp:pagebreak",
    b"meta",
    b"param",
    b"source",
    b"track",
    b"wbr",
];

/// Keep only the pagebreaks that separate **top-level siblings** — the ones
/// where no element is open around them.
///
/// A MOBI6 body is one HTML stream and `<mbp:pagebreak>` is a rendering hint,
/// not a structural boundary: it appears wherever kindlegen wanted a new page,
/// including in the middle of an element. Cutting there splits that element
/// across two XHTML documents, so one chapter ends with an unclosed open tag
/// and the next begins with an orphaned close — a document that is not
/// well-formed (epubcheck RSC-016), plus a content-free husk chapter holding
/// nothing but the stray close tag.
///
/// Periodicals do this on every article: the separator sits just inside the
/// article's `<block>`, immediately before `</block>`. Filtering to depth zero
/// moves the cut to the real article boundary and the husks stop existing.
///
/// Returns `None` when no pagebreak qualifies, leaving the caller to fall back
/// to the unfiltered set — a book whose whole body sits inside one wrapper
/// element has no depth-zero break at all and must still split somewhere.
fn pagebreaks_at_top_level(body: &[u8], all: &[PagebreakPos]) -> Option<Vec<PagebreakPos>> {
    // Element depth immediately before each byte offset of interest, walked
    // once over the body.
    let mut depth: i32 = 0;
    let mut kept = Vec::new();
    let mut next = all.iter().peekable();
    let mut pos = 0;

    while pos < body.len() {
        // Record the depth at each pagebreak as its offset is reached.
        while let Some(pb) = next.peek() {
            if pb.start > pos {
                break;
            }
            if depth == 0 {
                kept.push(PagebreakPos {
                    start: pb.start,
                    end: pb.end,
                });
            }
            next.next();
        }

        if body[pos] != b'<' {
            pos += 1;
            continue;
        }
        let Some(close_rel) = body[pos..].iter().position(|&b| b == b'>') else {
            break;
        };
        let tag = &body[pos..pos + close_rel + 1];
        pos += close_rel + 1;

        // `<!…>` and `<?…>` are not elements.
        let is_close = tag.get(1) == Some(&b'/');
        let name_start = if is_close { 2 } else { 1 };
        if !tag.get(name_start).is_some_and(|b| b.is_ascii_alphabetic()) {
            continue;
        }
        let name_end = tag[name_start..]
            .iter()
            .position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b':' | b'-' | b'_')))
            .map_or(tag.len(), |p| name_start + p);
        let name = &tag[name_start..name_end];
        if VOID_ELEMENTS.iter().any(|v| v.eq_ignore_ascii_case(name)) {
            continue;
        }
        // `<foo/>` opens and closes in one tag.
        let self_closing = tag.len() >= 2 && tag[tag.len() - 2] == b'/';
        if is_close {
            depth = depth.saturating_sub(1);
        } else if !self_closing {
            depth += 1;
        }
    }

    // Any pagebreaks past the last tag sit at whatever depth the walk ended on.
    for pb in next {
        if depth == 0 {
            kept.push(PagebreakPos {
                start: pb.start,
                end: pb.end,
            });
        }
    }

    (!kept.is_empty()).then_some(kept)
}

/// Split body content at pagebreak positions.
///
/// The pagebreak tags themselves are removed. Content before the first
/// pagebreak becomes the first chunk, etc.
fn split_at_pagebreaks(body: &str, pagebreaks: &[PagebreakPos]) -> Vec<String> {
    let mut chunks = Vec::with_capacity(pagebreaks.len() + 1);
    let mut last_end = 0;

    for pb in pagebreaks {
        chunks.push(body[last_end..pb.start].to_string());
        last_end = pb.end;
    }

    // Content after the last pagebreak
    chunks.push(body[last_end..].to_string());

    chunks
}

/// Fold chunks that hold nothing a reader could see into their neighbour.
///
/// Splitting one HTML stream leaves filler between boundaries: MOBI6 puts a
/// `<p> </p>` spacer between consecutive page breaks, so a chunk can consist
/// of markup and no content at all. Each one would otherwise become a chapter
/// — a real spine entry and a real "page" the reader can turn to, showing
/// nothing. Periodicals produce a run of them between cartoons.
///
/// Folded rather than dropped: an empty-looking chunk can still carry the
/// `<a id="fileposN"/>` anchor a TOC entry targets, and appending its markup to
/// the previous chapter keeps that anchor reachable. The leading chunk has no
/// previous, so it folds forward instead.
///
/// A chunk counts as content when it has text (ignoring the zero-width and
/// non-breaking spaces MOBI uses as spacers) or embeds media.
fn coalesce_contentless_chunks(chunks: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(chunks.len());
    let mut pending = String::new();
    for chunk in chunks {
        if chunk_has_content(&chunk) {
            out.push(std::mem::take(&mut pending) + &chunk);
        } else if let Some(last) = out.last_mut() {
            last.push_str(&chunk);
        } else {
            // Nothing before it yet — carry it onto the first real chunk.
            pending.push_str(&chunk);
        }
    }
    // Trailing filler with no chapter to attach to, or an all-filler body.
    if !pending.is_empty() {
        out.push(pending);
    }
    out
}

/// Does this chunk show the reader anything — text, or embedded media?
fn chunk_has_content(chunk: &str) -> bool {
    const MEDIA: [&str; 5] = ["<img", "<image", "<svg", "<audio", "<video"];
    let lower = chunk.to_ascii_lowercase();
    if MEDIA.iter().any(|m| lower.contains(m)) {
        return true;
    }
    let mut in_tag = false;
    chunk.chars().any(|c| {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            // U+200B zero-width space and U+00A0 no-break space are spacers
            // in MOBI6 markup, not content.
            _ if !in_tag && !c.is_whitespace() && c != '\u{200b}' && c != '\u{a0}' => return true,
            _ => {}
        }
        false
    })
}

/// Replace every `<mbp:pagebreak>` left inside a chunk with a page-break div.
///
/// Only the pagebreaks chosen as chapter boundaries are consumed by the split;
/// the rest stay in the text. `mbp:` is not a bound namespace prefix in an
/// XHTML chapter, so each survivor is a namespace-well-formedness error
/// (epubcheck RSC-016) — and dropping them outright would lose a page break the
/// publisher asked for.
///
/// Mirrors calibre's `MobiReader.replace_page_breaks`
/// (`reader/mobi6.py:841`), which rewrites the same tag to
/// `<div class="mbp_pagebreak" />` against a `page-break-after: always; margin:
/// 0; display: block` rule, and collapses a run of adjacent pagebreaks into
/// one. The rule is inlined here because the passthrough export route ships no
/// stylesheet of its own. Calibre carries the tag's original attributes onto
/// the div and cleans them up later; this route has no such pass, so they are
/// dropped rather than left to fail attribute validation.
fn replace_leftover_pagebreaks(chunk: &str) -> String {
    const DIV: &str = "<div style=\"page-break-after: always; margin: 0; display: block\"></div>";
    let breaks = find_pagebreaks(chunk.as_bytes());
    if breaks.is_empty() {
        return chunk.to_string();
    }
    let mut out = String::with_capacity(chunk.len());
    let mut last_end = 0;
    let mut iter = breaks.iter().peekable();
    while let Some(pb) = iter.next() {
        out.push_str(&chunk[last_end..pb.start]);
        let mut end = pb.end;
        // Collapse a run: further pagebreaks separated only by whitespace.
        while let Some(next) = iter.peek() {
            if !chunk[end..next.start].trim().is_empty() {
                break;
            }
            end = next.end;
            iter.next();
        }
        out.push_str(DIV);
        last_end = end;
    }
    out.push_str(&chunk[last_end..]);
    out
}

/// Scan a chapter chunk for `<a id="fileposN"` anchors and record them.
fn collect_filepos_anchors(chunk: &str, chapter_idx: usize, map: &mut HashMap<String, usize>) {
    let needle = "id=\"filepos";
    let mut search_pos = 0;

    while let Some(rel) = chunk[search_pos..].find(needle) {
        let value_start = search_pos + rel + needle.len();
        // Read digits until closing quote
        let value_end = chunk[value_start..]
            .find('"')
            .map(|p| value_start + p)
            .unwrap_or(value_start);

        if value_end > value_start {
            let filepos_key = format!("filepos{}", &chunk[value_start..value_end]);
            map.insert(filepos_key, chapter_idx);
        }

        search_pos = value_end + 1;
        if search_pos >= chunk.len() {
            break;
        }
    }
}

/// Rewrite `href="#fileposN"` links that point to anchors in other chapters.
///
/// If the target filepos is in a different chapter, rewrites to
/// `href="chapter_M.xhtml#fileposN"`.
fn rewrite_cross_chapter_links(
    chapters: &mut [Vec<u8>],
    filepos_to_chapter: &HashMap<String, usize>,
    chapter_paths: &[String],
) {
    let needle = b"href=\"#filepos";

    for (chapter_idx, chapter) in chapters.iter_mut().enumerate() {
        let mut output = Vec::with_capacity(chapter.len());
        let mut pos = 0;

        while pos < chapter.len() {
            if pos + needle.len() < chapter.len() && chapter[pos..].starts_with(needle) {
                // Found href="#filepos...", extract the filepos key
                let value_start = pos + b"href=\"#".len();
                let quote_end = chapter[value_start..]
                    .iter()
                    .position(|&b| b == b'"')
                    .map(|p| value_start + p);

                if let Some(end) = quote_end {
                    let filepos_key =
                        String::from_utf8_lossy(&chapter[value_start..end]).to_string();
                    let target_chapter = filepos_to_chapter
                        .get(&filepos_key)
                        .copied()
                        .unwrap_or(chapter_idx);

                    if target_chapter != chapter_idx {
                        // Cross-chapter link: rewrite
                        output.extend_from_slice(b"href=\"");
                        output.extend_from_slice(chapter_paths[target_chapter].as_bytes());
                        output.push(b'#');
                        output.extend_from_slice(filepos_key.as_bytes());
                        output.push(b'"');
                    } else {
                        // Same chapter: keep as-is
                        output.extend_from_slice(&chapter[pos..end + 1]);
                    }
                    pos = end + 1;
                    continue;
                }
            }

            output.push(chapter[pos]);
            pos += 1;
        }

        *chapter = output;
    }
}

/// Split body content at NCX anchor positions.
///
/// Finds `id="fileposN"` attributes in the body for each NCX position,
/// locates the enclosing `<a` tag, and splits just before it.
/// Content before the first anchor becomes the first chunk (preamble/front matter).
///
/// Handles both inserted anchors (`<a id="fileposN" />`) and pre-existing
/// anchors where `id` isn't the first attribute (`<a class="c1" id="fileposN">`).
fn split_at_ncx_anchors(body: &str, positions: &[u32]) -> Vec<String> {
    if positions.is_empty() {
        return vec![body.to_string()];
    }

    let body_bytes = body.as_bytes();

    // Find byte offsets of each NCX anchor in the body
    let mut split_offsets = Vec::new();
    for &pos in positions {
        let needle = format!("id=\"filepos{}\"", pos);
        if let Some(id_offset) = body.find(&needle) {
            // Scan backward to find the opening '<' of the enclosing tag
            let tag_start = body_bytes[..id_offset]
                .iter()
                .rposition(|&b| b == b'<')
                .unwrap_or(id_offset);
            if tag_start > 0 {
                split_offsets.push(tag_start);
            }
        }
    }

    split_offsets.sort_unstable();
    split_offsets.dedup();

    if split_offsets.is_empty() {
        return vec![body.to_string()];
    }

    let mut chunks = Vec::with_capacity(split_offsets.len() + 1);
    let mut last_end = 0;

    for &offset in &split_offsets {
        if offset > last_end {
            chunks.push(body[last_end..offset].to_string());
        }
        last_end = offset;
    }

    // Content after the last split point
    if last_end < body.len() {
        chunks.push(body[last_end..].to_string());
    }

    chunks
}

/// Neutralize bare filename links that reference OEB source files.
///
/// Some older MOBI files retain original OEB package filenames as `href` values
/// (e.g. `HREF="cover.htm"`, `HREF="Book_oeb_01_r1.html"`). These use uppercase
/// `HREF` and coexist with a lowercase `href="#fileposN"` on the same tag.
/// Since HTML parsers take the first attribute, the uppercase OEB link wins.
///
/// This function removes the entire `HREF="filename.html"` attribute (case-
/// insensitive) when it points to a bare filename, letting the correct lowercase
/// `href="#fileposN"` take effect. Falls back to replacing with `href="#"` if
/// there's only one href attribute.
fn neutralize_bare_filename_links(chapters: &mut [Vec<u8>]) {
    for chapter in chapters.iter_mut() {
        let mut output = Vec::with_capacity(chapter.len());
        let mut pos = 0;

        while pos < chapter.len() {
            // Case-insensitive match for href=" (handles HREF=", Href=", etc.)
            if pos + 6 <= chapter.len()
                && chapter[pos..pos + 5].eq_ignore_ascii_case(b"href=")
                && chapter[pos + 5] == b'"'
            {
                let value_start = pos + 6;
                if let Some(quote_rel) = chapter[value_start..].iter().position(|&b| b == b'"') {
                    let value = &chapter[value_start..value_start + quote_rel];
                    if is_bare_filename_link(value) {
                        let attr_end = value_start + quote_rel + 1; // past closing "

                        // Check if there's already a lowercase href on this tag
                        // by looking ahead in the same tag for href="# or href="chapter_
                        let remaining_tag = &chapter[attr_end..];
                        let has_correct_href = remaining_tag
                            .windows(6)
                            .take_while(|w| !w.starts_with(b">") && !w.starts_with(b"<"))
                            .any(|w| w == b"href=\"");

                        if has_correct_href {
                            // Remove the OEB HREF attribute entirely (skip it)
                            // Also skip trailing whitespace
                            pos = attr_end;
                            while pos < chapter.len() && chapter[pos] == b' ' {
                                pos += 1;
                            }
                            continue;
                        } else {
                            // No correct href follows — neutralize to href="#"
                            output.extend_from_slice(b"href=\"#\"");
                            pos = attr_end;
                            continue;
                        }
                    }
                }
            }

            output.push(chapter[pos]);
            pos += 1;
        }

        *chapter = output;
    }
}

/// Check if an href value is a bare filename link to an .htm/.html file.
///
/// Returns true for values like `cover.htm`, `Book_oeb_01_r1.html`,
/// `Book_oeb_ftn_r1.html#f1` (with fragment).
/// Returns false for `#filepos123`, `http://...`, `chapter_0.xhtml`, etc.
fn is_bare_filename_link(href: &[u8]) -> bool {
    let href_str = String::from_utf8_lossy(href);
    // Strip fragment for extension check
    let path_part = href_str.split('#').next().unwrap_or(&href_str);
    let path_lower = path_part.to_ascii_lowercase();

    (path_lower.ends_with(".htm") || path_lower.ends_with(".html"))
        && !href_str.starts_with('#')
        && !href_str.contains("://")
        && !path_lower.ends_with(".xhtml")
}

// ============================================================================
// Helpers
// ============================================================================

/// The asset stored `offset` records past `first_image_index`.
///
/// EXTH 201 (cover) and 202 (thumbnail) count raw records, including the
/// non-image ones — `RESC`, `DATP`, `FLIS`, `FCIS` — that asset discovery
/// filters out, so the offset is not a position in the asset list. Match on the
/// offset the filename encodes instead, the same index
/// [`MobiImporter::load_asset`] parses back out to read the record.
fn asset_at_record_offset(assets: &[PathBuf], offset: u32) -> Option<&PathBuf> {
    assets
        .iter()
        .find(|p| asset_record_offset(p) == Some(offset))
}

/// Discover asset paths by scanning image records (standalone function for early use).
fn discover_assets_from_source(
    source: &Arc<dyn ByteSource>,
    pdb: &PdbInfo,
    mobi: &MobiHeader,
    file_len: u64,
) -> Vec<PathBuf> {
    let mut assets = Vec::new();

    if mobi.first_image_index == NULL_INDEX {
        return assets;
    }

    let first_img = mobi.first_image_index as usize;
    for i in first_img..pdb.num_records as usize {
        if let Ok((start, end)) = pdb.record_range(i, file_len) {
            let read_len = 16.min((end - start) as usize);
            let mut header = [0u8; 16];
            if source.read_at_into(start, &mut header[..read_len]).is_ok() {
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

/// Wrap raw text as HTML.
fn wrap_text_as_html(text: &[u8], title: &str, mobi: &MobiHeader) -> Vec<u8> {
    let charset = match mobi.encoding {
        Encoding::Utf8 => "utf-8",
        _ => "windows-1252",
    };

    let content = String::from_utf8_lossy(text);
    let content_str = content.trim();

    // Check if content already has HTML structure
    if content_str.starts_with("<!DOCTYPE") || content_str.starts_with("<html") {
        return text.to_vec();
    }

    // Wrap as HTML
    let html = format!(
        r#"<?xml version="1.0" encoding="{charset}"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml">
<head>
<title>{title}</title>
<meta charset="{charset}"/>
</head>
<body>
{content}
</body>
</html>"#,
        charset = charset,
        title = html_escape(title),
        content = content,
    );

    html.into_bytes()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Chapter path for the generated issue contents page. Distinct from the
/// `chapter_N.xhtml` run so inserting it renames nothing.
const ISSUE_FRONT_MATTER_PATH: &str = "issue.xhtml";

/// Wrap generated front matter in the same XHTML skeleton the split gives
/// every other chapter.
fn wrap_front_matter(body: &str, title: &str) -> Vec<u8> {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <!DOCTYPE html>\n\
         <html xmlns=\"http://www.w3.org/1999/xhtml\">\n\
         <head>\n<title>{}</title>\n</head>\n\
         <body>\n{}</body>\n\
         </html>",
        html_escape(title),
        body
    )
    .into_bytes()
}

/// Convert TocNode to TocEntry recursively.
fn toc_node_to_entry(node: TocNode) -> TocEntry {
    let mut entry = TocEntry::new(&node.title, &node.href);
    entry.children = node.children.into_iter().map(toc_node_to_entry).collect();
    entry
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_head_and_body() {
        let html = r#"<html><head><title>Test</title><link rel="stylesheet" href="style.css"/></head><body><p>Hello</p></body></html>"#;
        let (head, body) = extract_head_and_body(html);
        assert!(head.contains("<title>Test</title>"));
        assert!(head.contains("style.css"));
        assert_eq!(body, "<p>Hello</p>");
    }

    #[test]
    fn test_extract_head_and_body_no_tags() {
        let html = "<p>Just content</p>";
        let (head, body) = extract_head_and_body(html);
        assert!(head.is_empty());
        assert_eq!(body, html);
    }

    #[test]
    fn test_find_pagebreaks() {
        let body = b"<p>Ch1</p><mbp:pagebreak/><p>Ch2</p><mbp:pagebreak /><p>Ch3</p>";
        let pbs = find_pagebreaks(body);
        assert_eq!(pbs.len(), 2);
        assert_eq!(&body[pbs[0].start..pbs[0].end], b"<mbp:pagebreak/>");
        assert_eq!(&body[pbs[1].start..pbs[1].end], b"<mbp:pagebreak />");
    }

    #[test]
    fn test_find_pagebreaks_case_insensitive() {
        let body = b"<p>A</p><MBP:PAGEBREAK/><p>B</p>";
        let pbs = find_pagebreaks(body);
        assert_eq!(pbs.len(), 1);
    }

    #[test]
    fn test_find_pagebreaks_with_attributes() {
        let body = b"<p>A</p><mbp:pagebreak kindle:kindlefix=\"true\"/><p>B</p>";
        let pbs = find_pagebreaks(body);
        assert_eq!(pbs.len(), 1);
    }

    #[test]
    fn test_find_pagebreaks_none() {
        let body = b"<p>No breaks here</p>";
        let pbs = find_pagebreaks(body);
        assert!(pbs.is_empty());
    }

    #[test]
    fn test_split_at_pagebreaks() {
        let body = "<p>Ch1</p><mbp:pagebreak/><p>Ch2</p><mbp:pagebreak /><p>Ch3</p>";
        let pbs = find_pagebreaks(body.as_bytes());
        let chunks = split_at_pagebreaks(body, &pbs);

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], "<p>Ch1</p>");
        assert_eq!(chunks[1], "<p>Ch2</p>");
        assert_eq!(chunks[2], "<p>Ch3</p>");
    }

    #[test]
    fn test_sanitize_mobi_head() {
        // The MOBI `<guide>` block is an OPF concept — stripped; a `<title>`
        // is injected when the source head lacks one.
        let head =
            "<guide><reference type=\"toc\" title=\"目次\" href=\"#filepos418\" /></guide>\n";
        let out = sanitize_mobi_head(head, "人間<失格>");
        assert!(!out.contains("guide"), "guide stripped: {out}");
        assert!(
            out.contains("<title>人間&lt;失格&gt;</title>"),
            "title injected + escaped: {out}"
        );

        // A head that already has a title keeps it, no duplicate.
        let head2 = "<title>Keep</title>";
        let out2 = sanitize_mobi_head(head2, "Other");
        assert_eq!(out2.matches("<title").count(), 1);
        assert!(out2.contains("Keep"));
    }

    #[test]
    fn test_split_mobi_html_with_pagebreaks() {
        let html = br#"<html><head><title>T</title></head><body>
<h1>Chapter 1</h1><p>Text1</p>
<mbp:pagebreak/>
<h1>Chapter 2</h1><p>Text2</p>
<mbp:pagebreak/>
<h1>Chapter 3</h1><p>Text3</p>
</body></html>"#;

        let split = split_mobi_html(html, None, "T");

        assert_eq!(split.chapters.len(), 3);
        assert_eq!(split.chapter_paths.len(), 3);
        assert_eq!(split.chapter_paths[0], "chapter_0.xhtml");
        assert_eq!(split.chapter_paths[1], "chapter_1.xhtml");
        assert_eq!(split.chapter_paths[2], "chapter_2.xhtml");

        // Each chapter should be a complete XHTML document
        for ch in &split.chapters {
            let s = String::from_utf8_lossy(ch);
            assert!(s.contains("<html"), "Missing <html>: {}", s);
            assert!(s.contains("</html>"), "Missing </html>: {}", s);
            assert!(s.contains("<head>"), "Missing <head>: {}", s);
            assert!(s.contains("<body>"), "Missing <body>: {}", s);
        }

        // Check content
        let ch0 = String::from_utf8_lossy(&split.chapters[0]);
        let ch1 = String::from_utf8_lossy(&split.chapters[1]);
        let ch2 = String::from_utf8_lossy(&split.chapters[2]);
        assert!(ch0.contains("Chapter 1"));
        assert!(ch1.contains("Chapter 2"));
        assert!(ch2.contains("Chapter 3"));
    }

    #[test]
    fn test_split_mobi_html_no_pagebreaks() {
        let html = b"<html><head></head><body><p>Single chapter</p></body></html>";
        let split = split_mobi_html(html, None, "T");

        assert_eq!(split.chapters.len(), 1);
        assert_eq!(split.chapter_paths[0], "chapter_0.xhtml");

        let ch = String::from_utf8_lossy(&split.chapters[0]);
        assert!(ch.contains("Single chapter"));
    }

    #[test]
    fn test_split_mobi_html_empty_chunks_filtered() {
        // Pagebreak at very start → first chunk is empty → filtered out
        let html = b"<html><head></head><body><mbp:pagebreak/><p>Only chapter</p></body></html>";
        let split = split_mobi_html(html, None, "T");

        assert_eq!(split.chapters.len(), 1);
        let ch = String::from_utf8_lossy(&split.chapters[0]);
        assert!(ch.contains("Only chapter"));
    }

    #[test]
    fn test_collect_filepos_anchors() {
        let chunk = r#"<a id="filepos100" /><p>Text</p><a id="filepos500" />"#;
        let mut map = HashMap::new();
        collect_filepos_anchors(chunk, 2, &mut map);

        assert_eq!(map.get("filepos100"), Some(&2));
        assert_eq!(map.get("filepos500"), Some(&2));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn test_cross_chapter_link_rewriting() {
        // Chapter 0 has filepos100, Chapter 1 has filepos500
        let ch0 = concat!(
            "<html><body>",
            "<a id=\"filepos100\" />",
            "<a href=\"#filepos100\">self</a>",
            "<a href=\"#filepos500\">cross</a>",
            "</body></html>",
        );
        let ch1 = concat!(
            "<html><body>",
            "<a id=\"filepos500\" />",
            "<p>Ch2</p>",
            "</body></html>",
        );
        let mut chapters = vec![ch0.as_bytes().to_vec(), ch1.as_bytes().to_vec()];

        let mut map = HashMap::new();
        map.insert("filepos100".to_string(), 0);
        map.insert("filepos500".to_string(), 1);

        let paths = vec!["chapter_0.xhtml".to_string(), "chapter_1.xhtml".to_string()];

        rewrite_cross_chapter_links(&mut chapters, &map, &paths);

        let ch0 = String::from_utf8_lossy(&chapters[0]);
        // Same-chapter link should be unchanged
        assert!(
            ch0.contains(r##"href="#filepos100""##),
            "Same-chapter link should be unchanged: {}",
            ch0
        );
        // Cross-chapter link should be rewritten
        assert!(
            ch0.contains(r##"href="chapter_1.xhtml#filepos500""##),
            "Cross-chapter link should be rewritten: {}",
            ch0
        );
    }

    #[test]
    fn test_head_content_shared_across_chapters() {
        let html =
            br#"<html><head><title>Book</title><link rel="stylesheet" href="s.css"/></head><body>
<p>Ch1</p><mbp:pagebreak/><p>Ch2</p>
</body></html>"#;

        let split = split_mobi_html(html, None, "T");

        assert_eq!(split.chapters.len(), 2);
        for ch in &split.chapters {
            let s = String::from_utf8_lossy(ch);
            assert!(
                s.contains("<title>Book</title>"),
                "Head should contain title: {}",
                s
            );
            assert!(
                s.contains("s.css"),
                "Head should contain stylesheet link: {}",
                s
            );
        }
    }

    #[test]
    fn test_filepos_to_chapter_mapping() {
        let html = br#"<html><head></head><body>
<a id="filepos10" /><p>Ch1</p>
<mbp:pagebreak/>
<a id="filepos200" /><p>Ch2</p>
<mbp:pagebreak/>
<a id="filepos500" /><p>Ch3</p>
</body></html>"#;

        let split = split_mobi_html(html, None, "T");

        assert_eq!(split.filepos_to_chapter.get("filepos10"), Some(&0));
        assert_eq!(split.filepos_to_chapter.get("filepos200"), Some(&1));
        assert_eq!(split.filepos_to_chapter.get("filepos500"), Some(&2));
    }

    #[test]
    fn test_toc_uses_chapter_paths() {
        // Simulate what from_source does: build TOC with chapter paths
        let html = br#"<html><head></head><body>
<a id="filepos0" /><p>Ch1</p>
<mbp:pagebreak/>
<a id="filepos100" /><p>Ch2</p>
</body></html>"#;

        let split = split_mobi_html(html, None, "T");

        // Simulate NCX-based TOC construction
        let filepos0_ch = split
            .filepos_to_chapter
            .get("filepos0")
            .copied()
            .unwrap_or(0);
        let filepos100_ch = split
            .filepos_to_chapter
            .get("filepos100")
            .copied()
            .unwrap_or(0);

        let href0 = format!("{}#filepos0", split.chapter_paths[filepos0_ch]);
        let href1 = format!("{}#filepos100", split.chapter_paths[filepos100_ch]);

        assert_eq!(href0, "chapter_0.xhtml#filepos0");
        assert_eq!(href1, "chapter_1.xhtml#filepos100");
    }

    // ====================================================================
    // NCX fallback splitting tests
    // ====================================================================

    #[test]
    fn test_split_ncx_fallback_basic() {
        // HTML without pagebreaks but with filepos anchors at NCX positions
        let html = br#"<html><head><title>Book</title></head><body>
<a id="filepos0" /><h1>Preamble</h1><p>Front matter</p>
<a id="filepos100" /><h1>Chapter 1</h1><p>Text1</p>
<a id="filepos500" /><h1>Chapter 2</h1><p>Text2</p>
</body></html>"#;

        let ncx_positions = vec![0, 100, 500];
        let split = split_mobi_html(html, Some(&ncx_positions), "T");

        // Should split at filepos100 and filepos500 (filepos0 is at body start, skipped)
        assert_eq!(split.chapters.len(), 3);
        assert_eq!(split.chapter_paths[0], "chapter_0.xhtml");
        assert_eq!(split.chapter_paths[1], "chapter_1.xhtml");
        assert_eq!(split.chapter_paths[2], "chapter_2.xhtml");

        let ch0 = String::from_utf8_lossy(&split.chapters[0]);
        let ch1 = String::from_utf8_lossy(&split.chapters[1]);
        let ch2 = String::from_utf8_lossy(&split.chapters[2]);
        assert!(
            ch0.contains("Preamble"),
            "Ch0 should have preamble: {}",
            ch0
        );
        assert!(
            ch1.contains("Chapter 1"),
            "Ch1 should have Chapter 1: {}",
            ch1
        );
        assert!(
            ch2.contains("Chapter 2"),
            "Ch2 should have Chapter 2: {}",
            ch2
        );
    }

    #[test]
    fn test_split_ncx_fallback_filepos_to_chapter_map() {
        let html = br#"<html><head></head><body>
<a id="filepos0" /><p>Preamble</p>
<a id="filepos200" /><h1>Ch1</h1><a id="filepos300" /><p>More ch1</p>
<a id="filepos800" /><h1>Ch2</h1>
</body></html>"#;

        // Only split at 200 and 800 (skip sub-position 300)
        let ncx_positions = vec![0, 200, 800];
        let split = split_mobi_html(html, Some(&ncx_positions), "T");

        assert_eq!(split.chapters.len(), 3);

        // filepos300 should be in chapter 1 (same chapter as filepos200)
        assert_eq!(split.filepos_to_chapter.get("filepos0"), Some(&0));
        assert_eq!(split.filepos_to_chapter.get("filepos200"), Some(&1));
        assert_eq!(split.filepos_to_chapter.get("filepos300"), Some(&1));
        assert_eq!(split.filepos_to_chapter.get("filepos800"), Some(&2));
    }

    #[test]
    fn test_split_ncx_no_matching_anchors() {
        // NCX positions that don't match any filepos anchors → single chapter
        let html = b"<html><head></head><body><p>No anchors here</p></body></html>";

        let ncx_positions = vec![100, 200, 300];
        let split = split_mobi_html(html, Some(&ncx_positions), "T");

        assert_eq!(split.chapters.len(), 1);
    }

    #[test]
    fn test_split_ncx_empty_positions() {
        let html = b"<html><head></head><body><p>Content</p></body></html>";

        let ncx_positions: Vec<u32> = vec![];
        let split = split_mobi_html(html, Some(&ncx_positions), "T");

        assert_eq!(split.chapters.len(), 1);
    }

    #[test]
    fn test_split_prefers_top_level_pagebreaks() {
        // The periodical shape: each article is a `<block>` whose page break
        // sits just inside it, before `</block>`, followed by a top-level
        // break between articles. Cutting on the inner break strands the
        // `</block>` in a content-free husk chapter; cutting on the outer one
        // lands on the real article boundary.
        let html = br#"<html><head></head><body>
<block><p>Article one</p><mbp:pagebreak/></block><p> </p><mbp:pagebreak/>
<block><p>Article two</p><mbp:pagebreak/></block><p> </p>
</body></html>"#;

        let split = split_mobi_html(html, None, "T");

        assert_eq!(split.chapters.len(), 2, "one chapter per article");
        for (i, ch) in split.chapters.iter().enumerate() {
            let s = String::from_utf8_lossy(ch);
            assert_eq!(
                s.matches("<block").count(),
                s.matches("</block>").count(),
                "chapter {i} keeps <block> balanced: {s}"
            );
            assert!(
                !s.contains("mbp:"),
                "chapter {i} carries no unbound mbp: prefix: {s}"
            );
        }
        assert!(String::from_utf8_lossy(&split.chapters[0]).contains("Article one"));
        assert!(String::from_utf8_lossy(&split.chapters[1]).contains("Article two"));
    }

    #[test]
    fn test_split_falls_back_when_no_top_level_pagebreak() {
        // A book whose whole body sits inside one wrapper has no depth-zero
        // break at all. It must still split — losing the chapter structure
        // would be worse than an unbalanced cut.
        let html = br#"<html><head></head><body><div>
<p>One</p><mbp:pagebreak/><p>Two</p><mbp:pagebreak/><p>Three</p>
</div></body></html>"#;

        let split = split_mobi_html(html, None, "T");
        assert_eq!(split.chapters.len(), 3, "falls back to every pagebreak");
    }

    #[test]
    fn test_asset_at_record_offset() {
        // Record +0 is a `RESC` that asset discovery filters out, so the
        // surviving assets start at raw +1 and the list position trails the
        // record offset by one from there on.
        let assets: Vec<PathBuf> = (1..=19)
            .map(|i| PathBuf::from(format!("images/image_{i:04}.jpg")))
            .collect();

        // EXTH 201 said 18 — the 221 KB cover, not the 15.8 KB thumbnail at 19
        // that plain positional indexing (`assets[18]`) returns.
        assert_eq!(
            asset_at_record_offset(&assets, 18),
            Some(&PathBuf::from("images/image_0018.jpg"))
        );
        assert_eq!(assets[18], PathBuf::from("images/image_0019.jpg"));

        // A book whose images start at record +0 is unshifted, and the lookup
        // agrees with positional indexing there.
        let unshifted: Vec<PathBuf> = (0..3)
            .map(|i| PathBuf::from(format!("images/image_{i:04}.jpg")))
            .collect();
        assert_eq!(
            asset_at_record_offset(&unshifted, 0),
            Some(&unshifted[0]),
            "offset 0 is the first asset when nothing was skipped"
        );

        // An offset pointing at a record that was filtered out has no asset.
        assert_eq!(asset_at_record_offset(&assets, 0), None);
        assert_eq!(asset_at_record_offset(&assets, 99), None);

        // The extension is whatever the record's magic bytes said.
        let png = vec![PathBuf::from("images/image_0007.png")];
        assert_eq!(asset_at_record_offset(&png, 7), Some(&png[0]));
    }

    #[test]
    fn test_chunk_has_content() {
        assert!(chunk_has_content("<p>text</p>"));
        assert!(chunk_has_content("<img src=\"a.jpg\"/>"), "media counts");
        assert!(chunk_has_content("<div><SVG><rect/></SVG></div>"));

        // MOBI6's inter-pagebreak spacer, and its spacer characters.
        assert!(!chunk_has_content(" <p> </p>"));
        assert!(!chunk_has_content("<p>\u{200b}\u{a0}</p>"));
        assert!(!chunk_has_content("<a id=\"filepos99\" />"));
        assert!(!chunk_has_content(""));
        // Attribute text is not content.
        assert!(!chunk_has_content(
            "<div style=\"page-break-after: always\"></div>"
        ));
    }

    #[test]
    fn test_coalesce_contentless_chunks() {
        // The cartoon run: a spacer chunk between every pair of real ones.
        let chunks = ["<p>A</p>", " <p> </p>", "<p>B</p>", " <p> </p>"]
            .map(String::from)
            .to_vec();
        let out = coalesce_contentless_chunks(chunks);
        assert_eq!(out.len(), 2, "spacers folded away: {out:?}");
        assert!(out[0].contains('A') && out[1].contains('B'));
        // The trailing spacer joined the chapter before it.
        assert!(
            out[1].ends_with("<p> </p>"),
            "trailing spacer kept: {out:?}"
        );

        // A leading spacer has no previous chapter, so it folds forward and
        // its anchor stays reachable.
        let out = coalesce_contentless_chunks(
            ["<a id=\"filepos1\" />", "<p>A</p>"]
                .map(String::from)
                .to_vec(),
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("filepos1") && out[0].contains('A'));

        // An all-filler body still yields one chapter rather than none.
        let out = coalesce_contentless_chunks(vec![" <p> </p>".to_string()]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn test_replace_leftover_pagebreaks() {
        // A break the split did not consume becomes a page-break div; `mbp:`
        // is not a bound prefix in an XHTML chapter.
        let out = replace_leftover_pagebreaks("<p>a</p><mbp:pagebreak/><p>b</p>");
        assert!(!out.contains("mbp:"), "prefix gone: {out}");
        assert!(
            out.contains("page-break-after: always"),
            "intent kept: {out}"
        );
        assert_eq!(out.matches("<div").count(), 1);

        // A run separated only by whitespace collapses to one div, like
        // calibre's PAGE_BREAK_PAT does.
        let out =
            replace_leftover_pagebreaks("a<mbp:pagebreak/> <mbp:pagebreak/>\n<MBP:PAGEBREAK>b");
        assert_eq!(out.matches("<div").count(), 1, "run collapsed: {out}");
        assert!(
            out.starts_with('a') && out.ends_with('b'),
            "text kept: {out}"
        );

        // Nothing to do is a cheap no-op that preserves the input exactly.
        assert_eq!(replace_leftover_pagebreaks("<p>x</p>"), "<p>x</p>");
    }

    #[test]
    fn test_ncx_anchors_map_to_pagebreak_chapters() {
        // A periodical navigates purely through its index: the body carries
        // `<mbp:pagebreak>` separators but no `filepos=` links at all, so the
        // only anchors that can exist are the ones inserted at NCX positions.
        // Transform must insert them even when the pagebreak split wins, or
        // every NCX target resolves to chapter 0 (the `unwrap_or(0)` in
        // `from_source`'s TOC builder).
        let body = "<html><head></head><body><p>One</p><mbp:pagebreak/><p>Two</p><mbp:pagebreak/><p>Three</p></body></html>";
        // Byte offsets of the three `<p>` starts in the source above.
        let positions: Vec<u32> = ["<p>One", "<p>Two", "<p>Three"]
            .iter()
            .map(|needle| body.find(needle).unwrap() as u32)
            .collect();

        let transformed = filepos::transform_mobi_html(body.as_bytes(), &[], &positions);
        let split = split_mobi_html(&transformed, None, "T");

        assert_eq!(split.chapters.len(), 3, "pagebreaks still drive the split");
        for (i, pos) in positions.iter().enumerate() {
            assert_eq!(
                split.filepos_to_chapter.get(&format!("filepos{}", pos)),
                Some(&i),
                "NCX position {} should map to chapter {}, map: {:?}",
                pos,
                i,
                split.filepos_to_chapter
            );
        }
    }

    #[test]
    fn test_pagebreaks_preferred_over_ncx() {
        // When both pagebreaks and NCX positions exist, pagebreaks should be used
        let html = br#"<html><head></head><body>
<a id="filepos0" /><p>Ch1</p>
<mbp:pagebreak/>
<a id="filepos100" /><p>Ch2</p>
<mbp:pagebreak/>
<a id="filepos200" /><p>Ch3</p>
</body></html>"#;

        // Pass NCX positions that would create a different split
        let ncx_positions = vec![0, 200];
        let split = split_mobi_html(html, Some(&ncx_positions), "T");

        // Should get 3 chapters from pagebreaks, not 2 from NCX
        assert_eq!(split.chapters.len(), 3);
    }

    #[test]
    fn test_ncx_cross_chapter_links() {
        // NCX-split chapters should have cross-chapter links rewritten
        let html = br##"<html><head></head><body>
<a id="filepos0" /><a href="#filepos500">Go to Ch2</a><p>Ch1</p>
<a id="filepos500" /><a href="#filepos0">Back to Ch1</a><p>Ch2</p>
</body></html>"##;

        let ncx_positions = vec![0, 500];
        let split = split_mobi_html(html, Some(&ncx_positions), "T");

        assert_eq!(split.chapters.len(), 2);

        let ch0 = String::from_utf8_lossy(&split.chapters[0]);
        let ch1 = String::from_utf8_lossy(&split.chapters[1]);

        // Cross-chapter links should be rewritten
        assert!(
            ch0.contains(r##"href="chapter_1.xhtml#filepos500""##),
            "Ch0 cross-link should be rewritten: {}",
            ch0
        );
        assert!(
            ch1.contains(r##"href="chapter_0.xhtml#filepos0""##),
            "Ch1 cross-link should be rewritten: {}",
            ch1
        );
    }

    // ====================================================================
    // OEB filename link neutralization tests
    // ====================================================================

    #[test]
    fn test_neutralize_bare_filename_links() {
        let html = br#"<a href="cover.htm">Cover</a> and <a href="Book_oeb_01_r1.html">Ch1</a>"#;
        let mut chapters = vec![html.to_vec()];
        neutralize_bare_filename_links(&mut chapters);

        let result = String::from_utf8_lossy(&chapters[0]);
        assert!(
            result.contains(r##"href="#""##),
            "Bare .htm link should be neutralized: {}",
            result
        );
        assert!(
            !result.contains("cover.htm"),
            "Original .htm reference should be removed: {}",
            result
        );
        assert!(
            !result.contains("oeb_01_r1.html"),
            "Original .html reference should be removed: {}",
            result
        );
    }

    #[test]
    fn test_neutralize_preserves_filepos_links() {
        let html =
            br##"<a href="#filepos100">Ch1</a> and <a href="chapter_0.xhtml#filepos200">Ch2</a>"##;
        let mut chapters = vec![html.to_vec()];
        neutralize_bare_filename_links(&mut chapters);

        let result = String::from_utf8_lossy(&chapters[0]);
        assert!(
            result.contains(r##"href="#filepos100""##),
            "filepos link should be preserved: {}",
            result
        );
        assert!(
            result.contains("chapter_0.xhtml"),
            "xhtml link should be preserved: {}",
            result
        );
    }

    #[test]
    fn test_neutralize_preserves_xhtml_links() {
        let html = br#"<a href="chapter_1.xhtml">Link</a>"#;
        let mut chapters = vec![html.to_vec()];
        neutralize_bare_filename_links(&mut chapters);

        let result = String::from_utf8_lossy(&chapters[0]);
        assert!(
            result.contains("chapter_1.xhtml"),
            "xhtml link should be preserved: {}",
            result
        );
    }

    #[test]
    fn test_is_bare_filename_link_cases() {
        assert!(is_bare_filename_link(b"cover.htm"));
        assert!(is_bare_filename_link(b"Book_oeb_01_r1.html"));
        assert!(is_bare_filename_link(b"Cover.HTML"));
        assert!(is_bare_filename_link(b"file.HTM"));

        assert!(!is_bare_filename_link(b"#filepos100"));
        assert!(!is_bare_filename_link(b"chapter_0.xhtml"));
        assert!(!is_bare_filename_link(b"http://example.com/file.html"));
        assert!(!is_bare_filename_link(b"https://example.com/page.htm"));
        assert!(!is_bare_filename_link(b"#"));
        assert!(!is_bare_filename_link(b"image.jpg"));

        // Fragment handling
        assert!(is_bare_filename_link(b"Book_oeb_ftn_r1.html#f1"));
        assert!(is_bare_filename_link(b"cover.htm#section"));
        assert!(!is_bare_filename_link(b"chapter_0.xhtml#filepos100"));
    }

    #[test]
    fn test_neutralize_uppercase_href() {
        // Real MOBI pattern: uppercase HREF with OEB link + lowercase href with filepos
        let html = br##"<A HREF="Asim_oeb_tp_r1.html"  href="#filepos1129"> Title Page</A>"##;
        let mut chapters = vec![html.to_vec()];
        neutralize_bare_filename_links(&mut chapters);

        let result = String::from_utf8_lossy(&chapters[0]);
        assert!(
            !result.contains("oeb_tp_r1.html"),
            "Uppercase HREF OEB link should be removed: {}",
            result
        );
        assert!(
            result.contains(r##"href="#filepos1129""##),
            "Lowercase filepos href should be preserved: {}",
            result
        );
    }

    #[test]
    fn test_neutralize_uppercase_href_no_fallback() {
        // Uppercase HREF without a lowercase href fallback
        let html = br#"<A HREF="cover.htm"> Cover</A>"#;
        let mut chapters = vec![html.to_vec()];
        neutralize_bare_filename_links(&mut chapters);

        let result = String::from_utf8_lossy(&chapters[0]);
        assert!(
            !result.contains("cover.htm"),
            "OEB link should be neutralized: {}",
            result
        );
        assert!(
            result.contains(r##"href="#""##),
            "Should have fallback href: {}",
            result
        );
    }

    #[test]
    fn test_neutralize_href_with_fragment() {
        let html = br#"<a href="Book_oeb_ftn_r1.html#f1">Note</a>"#;
        let mut chapters = vec![html.to_vec()];
        neutralize_bare_filename_links(&mut chapters);

        let result = String::from_utf8_lossy(&chapters[0]);
        assert!(
            !result.contains("oeb_ftn_r1.html"),
            "OEB link with fragment should be neutralized: {}",
            result
        );
    }

    #[test]
    fn test_ncx_split_with_oeb_links_neutralized() {
        // Simulate a MOBI with NCX-split chapters and OEB filename links
        let html = br#"<html><head></head><body>
<a id="filepos0" /><a href="cover.htm">Cover</a>
<a href="Book_oeb_01_r1.html">Ch1</a>
<a href="Book_oeb_02_r1.html">Ch2</a>
<p>Preamble content</p>
<a id="filepos500" /><h1>Chapter 1</h1><p>Text1</p>
<a id="filepos1000" /><h1>Chapter 2</h1><p>Text2</p>
</body></html>"#;

        let ncx_positions = vec![0, 500, 1000];
        let split = split_mobi_html(html, Some(&ncx_positions), "T");

        assert_eq!(split.chapters.len(), 3);

        // OEB links in preamble should be neutralized
        let ch0 = String::from_utf8_lossy(&split.chapters[0]);
        assert!(
            !ch0.contains("cover.htm"),
            "OEB links should be neutralized: {}",
            ch0
        );
        assert!(
            !ch0.contains("oeb_01_r1.html"),
            "OEB links should be neutralized: {}",
            ch0
        );

        // Content should still be there
        assert!(ch0.contains("Cover"), "Link text should be preserved");
        assert!(ch0.contains("Ch1"), "Link text should be preserved");
    }
}
