//! EPUB format importer - handles all IO.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use zip::ZipArchive;

use crate::dom::Stylesheet;
use crate::epub::{
    parse_container_xml, parse_nav_landmarks, parse_nav_page_list, parse_nav_toc, parse_ncx,
    parse_opf, parse_opf_guide,
};
use crate::import::{
    ChapterId, Importer, SpineEntry, normalize_components, resolve_path_based_href,
};
use crate::io::{ByteSource, ByteSourceCursor, FileSource, MemorySource};
use crate::model::{
    AnchorTarget, Chapter, GlobalNodeId, Landmark, Metadata, NodeId, Role, TocEntry,
};
use crate::util::percent_decode;

/// EPUB format importer with random-access ZIP reading.
pub struct EpubImporter {
    /// Random-access byte source for the ZIP file.
    source: Arc<dyn ByteSource>,

    /// Cached ZIP entry locations: path -> ZipEntryLoc.
    zip_index: HashMap<String, ZipEntryLoc>,

    /// Book metadata.
    metadata: Metadata,

    /// Table of contents.
    toc: Vec<TocEntry>,

    /// Physical page-break list from `<nav epub:type="page-list">` (printed
    /// page number → content location). Flat; empty when the EPUB has none.
    page_list: Vec<TocEntry>,

    /// Landmarks (structural navigation points).
    landmarks: Vec<Landmark>,

    /// Reading order (spine).
    spine: Vec<SpineEntry>,

    /// Maps ChapterId -> ZIP path (e.g., "OEBPS/text/ch01.xhtml").
    spine_paths: Vec<String>,

    /// All asset paths in the ZIP.
    assets: Vec<PathBuf>,

    /// Cached parsed stylesheets.
    css_cache: HashMap<String, Stylesheet>,

    // --- Link resolution ---
    /// Maps path (without fragment) -> ChapterId
    path_to_chapter: HashMap<String, ChapterId>,

    /// Maps "path#id" -> GlobalNodeId for fragment resolution
    anchor_map: HashMap<String, GlobalNodeId>,

    /// Maps chapter path -> [(whitespace-stripped heading text, element id)] for
    /// every short id-bearing element. Used by [`resolve_toc`] to repair flat
    /// TOCs whose hrefs dropped the `#fragment` (a common calibre artifact: two
    /// episodes share one `part00NN.html`, both pointing at the file start).
    toc_heading_ids: HashMap<String, Vec<(String, String)>>,
}

#[derive(Clone, Copy)]
struct ZipEntryLoc {
    data_offset: u64,
    compressed_size: u64,
    uncompressed_size: u64,
    compression: u16, // 0 = Store, 8 = Deflate
}

impl Importer for EpubImporter {
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
        self.spine_paths.get(id.0 as usize).map(|s| s.as_str())
    }

    fn load_raw(&mut self, id: ChapterId) -> io::Result<Vec<u8>> {
        let path = self.spine_paths.get(id.0 as usize).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Chapter ID {} not found", id.0),
            )
        })?;
        self.read_entry(path)
    }

    fn list_assets(&self) -> &[PathBuf] {
        &self.assets
    }

    fn load_asset(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        let key = path.to_string_lossy().replace('\\', "/");
        self.read_entry(&key)
    }

    fn load_stylesheet(&mut self, path: &Path) -> Option<Stylesheet> {
        let key = path.to_string_lossy().replace('\\', "/");
        if let Some(sheet) = self.css_cache.get(&key) {
            return Some(sheet.clone());
        }
        let resolved = self.read_css_with_imports(path, &mut std::collections::HashSet::new())?;
        let sheet = Stylesheet::parse(&resolved);
        self.css_cache.insert(key, sheet.clone());
        Some(sheet)
    }

    fn index_anchors(&mut self, chapters: &[(ChapterId, Arc<Chapter>)]) {
        self.anchor_map.clear();
        self.toc_heading_ids.clear();

        for (chapter_id, chapter) in chapters {
            // Get the chapter's source path
            let chapter_path = match self.spine_paths.get(chapter_id.0 as usize) {
                Some(p) => p.split('#').next().unwrap_or(p),
                None => continue,
            };

            // Walk the chapter and record all nodes with IDs
            for node_id in chapter.iter_dfs() {
                if let Some(id) = chapter.semantics.id(node_id) {
                    let key = format!("{}#{}", chapter_path, id);
                    self.anchor_map
                        .insert(key, GlobalNodeId::new(*chapter_id, node_id));

                    // Also index the id-bearing element by its (short) heading
                    // text so a fragment-less TOC href can be repaired to it.
                    let text = collect_node_text(chapter, node_id, HEADING_TEXT_BYTE_CAP);
                    let normalized = strip_whitespace(&text);
                    if !normalized.is_empty() {
                        self.toc_heading_ids
                            .entry(chapter_path.to_string())
                            .or_default()
                            .push((normalized, id.to_string()));
                    }
                }
            }
        }
    }

    /// Repair flat TOCs. EPUB TOC hrefs are usually authoritative, but calibre
    /// (and some retail) EPUBs collapse several headings into one file and emit
    /// a `#fragment`-less href for each, so every entry in that file jumps to
    /// its top. The fragments the hrefs *should* carry exist as element ids in
    /// the content; we recover them by matching each fragment-less entry's
    /// label to a unique id-bearing element in the target file.
    fn resolve_toc(&mut self) {
        // Disjoint field borrows: the repair reads the heading index while
        // mutating the TOC tree.
        let Self {
            toc,
            toc_heading_ids,
            ..
        } = self;
        repair_flat_toc_fragments(toc, toc_heading_ids);
    }

    fn resolve_href(&self, from_chapter: ChapterId, href: &str) -> Option<AnchorTarget> {
        self.resolve_href_impl(from_chapter, href, false)
    }

    fn resolve_toc_href(&self, from_chapter: ChapterId, href: &str) -> Option<AnchorTarget> {
        self.resolve_href_impl(from_chapter, href, true)
    }
}

impl EpubImporter {
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
            |p| self.path_to_chapter.get(p).copied(),
            |k| self.anchor_map.get(k).copied(),
            chapter_fallback,
        )
    }
}

impl EpubImporter {
    /// Scan the ZIP central directory and cache each entry's byte location.
    /// Factored out of [`from_source`] so it can be retried against a repaired
    /// in-memory copy when the raw bytes trip the `zip` crate.
    fn scan_zip(
        source: &Arc<dyn ByteSource>,
    ) -> io::Result<(HashMap<String, ZipEntryLoc>, Vec<PathBuf>)> {
        let cursor = ByteSourceCursor::new(source.clone());
        let mut archive = ZipArchive::new(cursor)?;

        let mut zip_index = HashMap::new();
        let mut assets = Vec::new();

        for i in 0..archive.len() {
            let file = archive.by_index(i)?;
            let name = file.name().to_string();

            zip_index.insert(
                name.clone(),
                ZipEntryLoc {
                    data_offset: file.data_start().unwrap(),
                    compressed_size: file.compressed_size(),
                    uncompressed_size: file.size(),
                    compression: compression_to_u16(file.compression()),
                },
            );
            assets.push(PathBuf::from(name));
        }

        Ok((zip_index, assets))
    }

    /// Create an importer from a ByteSource.
    pub fn from_source(source: Arc<dyn ByteSource>) -> io::Result<Self> {
        // 1. Scan ZIP central directory and cache entry locations. A handful of
        //    EPUB producers (e.g. ScribdMpubToEpubConverter) emit spurious
        //    ZIP64 extra fields that the `zip` crate misreads as
        //    "Invalid local file header"; if the first scan fails, retry once
        //    on a repaired in-memory copy. See `epub::neutralize_spurious_zip64`.
        let (zip_index, assets, source) = match Self::scan_zip(&source) {
            Ok((zip_index, assets)) => (zip_index, assets, source),
            Err(first_err) => {
                let raw = source.read_at(0, source.len() as usize)?;
                match crate::epub::neutralize_spurious_zip64(&raw) {
                    Some(repaired) => {
                        let repaired: Arc<dyn ByteSource> = Arc::new(MemorySource::new(repaired));
                        let (zip_index, assets) = Self::scan_zip(&repaired)?;
                        (zip_index, assets, repaired)
                    }
                    None => return Err(first_err),
                }
            }
        };

        // 2. Find OPF path from container.xml. The `full-path` is a URI
        //    reference, so percent-decode it to the literal zip entry name.
        let container_bytes = read_entry(&source, &zip_index, "META-INF/container.xml")?;
        let opf_path = percent_decode(&parse_container_xml(&container_bytes)?);
        let opf_base = archive_dir_base(&opf_path);

        // 3. Parse OPF
        let opf_bytes = read_entry(&source, &zip_index, &opf_path)?;
        let hint_encoding = crate::util::extract_xml_encoding(&opf_bytes);
        let opf_str = crate::util::decode_text(&opf_bytes, hint_encoding);
        let opf = parse_opf(&opf_str)?;

        // 4. Build spine
        let mut spine = Vec::new();
        let mut spine_paths = Vec::new();

        for (i, spine_id) in opf.spine_ids.iter().enumerate() {
            if let Some((href, _media_type)) = opf.manifest.get(spine_id) {
                // Manifest hrefs are URI references (calibre escapes `!` in
                // `CR!….html` as `CR%21….html`); decode so the resolved path
                // matches the literal zip entry name.
                let full_path = format!("{}{}", opf_base, percent_decode(href));
                let size_estimate = zip_index
                    .get(&full_path)
                    .map(|loc| loc.compressed_size as usize)
                    .unwrap_or(0);

                spine.push(SpineEntry {
                    id: ChapterId(i as u32),
                    size_estimate,
                    page_spread: opf
                        .spine_properties
                        .get(spine_id)
                        .and_then(|p| crate::model::PageSpread::from_opf_properties(p)),
                });
                spine_paths.push(full_path);
            }
        }

        // Assets = non-spine resources (images / CSS / fonts / audio).
        // `scan_zip` collected every entry; drop the container structure
        // (mimetype, META-INF/*, the OPF) and the navigation documents the
        // exporters regenerate (NCX, nav doc), plus every spine chapter —
        // re-bundling chapters as loose assets made an epub→epub re-export
        // write each chapter twice (duplicate-zip-entry error).
        let mut assets = assets;
        {
            let spine_set: std::collections::HashSet<&str> =
                spine_paths.iter().map(|s| s.as_str()).collect();
            let ncx_path = opf
                .ncx_href
                .as_ref()
                .map(|h| format!("{}{}", opf_base, percent_decode(h)));
            let nav_path = opf
                .nav_href
                .as_ref()
                .map(|h| format!("{}{}", opf_base, percent_decode(h)));
            assets.retain(|p| {
                let name = p.to_string_lossy();
                name != "mimetype"
                    && !name.starts_with("META-INF/")
                    && name != opf_path
                    && Some(name.as_ref()) != ncx_path.as_deref()
                    && Some(name.as_ref()) != nav_path.as_deref()
                    && !spine_set.contains(name.as_ref())
            });
        }

        // 5. Parse TOC. The EPUB 3 nav doc is the authoritative TOC; the legacy
        // EPUB 2 NCX is a fallback. Retail Japanese EPUBs (Kadokawa/EBPAJ)
        // routinely ship BOTH — a full nav doc AND a stub NCX that lists only
        // cover/目次/奥付 — so we parse both and keep whichever is richer,
        // preferring the nav on a tie. The old NCX-first-unless-empty order made
        // those books lose every chapter: the 3-entry stub NCX shadowed the
        // 7-entry nav. A book with only one source gets that one; with neither,
        // the TOC is empty (a headings-only book is handled downstream).
        let read_toc = |href: Option<&String>, parse: fn(&str) -> io::Result<Vec<TocEntry>>| {
            let href = href?;
            let path = format!("{}{}", opf_base, percent_decode(href));
            let bytes = read_entry(&source, &zip_index, &path).ok()?;
            let hint_encoding = crate::util::extract_xml_encoding(&bytes);
            let text = crate::util::decode_text(&bytes, hint_encoding);
            let entries = parse(&text).ok()?;
            // Hrefs in a nav doc / NCX are relative to THAT document's directory,
            // not the OPF's. They coincide when the OPF and the nav/NCX share a
            // directory (calibre-style OEBPS/), but retail EPUBs that keep the
            // OPF+NCX at the archive root and the nav doc in a subdir (e.g.
            // `xhtml/nav.xhtml`) resolve against different bases — prepending
            // opf_base there leaves every fragment-less chapter href unmatched.
            let doc_base = archive_dir_base(&path);
            (!entries.is_empty()).then(|| prepend_base_to_toc(&entries, &doc_base))
        };
        let toc = {
            let ncx_toc = read_toc(opf.ncx_href.as_ref(), parse_ncx);
            let nav_toc = read_toc(opf.nav_href.as_ref(), parse_nav_toc);
            match (ncx_toc, nav_toc) {
                (Some(ncx), Some(nav)) => {
                    if count_toc_entries(&ncx) > count_toc_entries(&nav) {
                        ncx
                    } else {
                        nav
                    }
                }
                (Some(only), None) | (None, Some(only)) => only,
                (None, None) => Vec::new(),
            }
        };

        // 5b. Parse the physical page-list (`<nav epub:type="page-list">`) from
        // the same EPUB 3 nav doc. Base-prefixed exactly like the TOC so its
        // `cN.xhtml` / `cN.xhtml#page_M` hrefs resolve against chapter paths.
        // Amazon preserves this as a `page_list` nav_container ("go to page N",
        // citations); dropping it loses every printed page number.
        let page_list = read_toc(opf.nav_href.as_ref(), parse_nav_page_list).unwrap_or_default();

        // 6. Parse landmarks from EPUB 3 nav document
        let mut landmarks = if let Some(nav_href) = &opf.nav_href {
            let nav_path = format!("{}{}", opf_base, percent_decode(nav_href));
            // Landmark hrefs are relative to the nav doc's directory, not the
            // OPF's (see the TOC note above).
            let nav_base = archive_dir_base(&nav_path);
            if let Ok(nav_bytes) = read_entry(&source, &zip_index, &nav_path) {
                let hint_encoding = crate::util::extract_xml_encoding(&nav_bytes);
                let nav_str = crate::util::decode_text(&nav_bytes, hint_encoding);
                let mut parsed = parse_nav_landmarks(&nav_str)?;
                // Prepend base path to hrefs (nav uses relative paths) and
                // percent-decode so the targets match decoded chapter paths.
                for landmark in &mut parsed {
                    landmark.href = percent_decode(&landmark.href);
                    if !landmark.href.starts_with('#') && !landmark.href.is_empty() {
                        landmark.href = format!("{}{}", nav_base, landmark.href);
                    }
                }
                parsed
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        // 6b. Fall back to EPUB 2.0 `<guide>` entries when the nav doc had
        // none (or didn't exist). EPUB 2.0 books and calibre-style 3.0
        // OPFs both ship landmarks via `<guide>`, and the kfx_to_epub path
        // emits guide-only OPFs by design (so Apple Books renders them).
        // We merge missing types rather than wholesale replace, so a nav
        // doc that omitted some EPUB-2-only landmarks (or vice versa)
        // still gets the union.
        if let Ok(mut guide_marks) = parse_opf_guide(&opf_str) {
            for landmark in &mut guide_marks {
                landmark.href = percent_decode(&landmark.href);
                if !landmark.href.starts_with('#') && !landmark.href.is_empty() {
                    landmark.href = format!("{}{}", opf_base, landmark.href);
                }
            }
            for g in guide_marks {
                if !landmarks.iter().any(|l| l.landmark_type == g.landmark_type) {
                    landmarks.push(g);
                }
            }
        }

        // Build path -> ChapterId map
        let mut path_to_chapter = HashMap::new();
        for (i, path) in spine_paths.iter().enumerate() {
            // Store path without fragment
            let base_path = path.split('#').next().unwrap_or(path);
            path_to_chapter.insert(base_path.to_string(), ChapterId(i as u32));
        }

        // Resolve cover_image to an absolute (zip-relative) path so it matches
        // asset keys downstream. The OPF parser leaves it as a manifest href
        // relative to opf_base; percent-decode it the same way as every other
        // href so a cover whose filename contains escaped characters resolves.
        let mut metadata = opf.metadata;
        if let Some(ref href) = metadata.cover_image
            && !href.is_empty()
        {
            metadata.cover_image = Some(format!("{}{}", opf_base, percent_decode(href)));
        }

        Ok(Self {
            source,
            zip_index,
            metadata,
            toc,
            page_list,
            landmarks,
            spine,
            spine_paths,
            assets,
            path_to_chapter,
            anchor_map: HashMap::new(),
            css_cache: HashMap::new(),
            toc_heading_ids: HashMap::new(),
        })
    }

    /// Read and decompress a ZIP entry by path.
    fn read_entry(&self, path: &str) -> io::Result<Vec<u8>> {
        read_entry(&self.source, &self.zip_index, path)
    }

    /// Read a CSS file and inline any `@import` rules so the parser sees a
    /// single flat stylesheet. boko's CSS parser silently skips at-rules
    /// other than @font-face; many Japanese EPUBs use `@import` heavily
    /// (e.g. book-style.css imports style-standard.css where vertical
    /// writing-mode lives) so without this step those rules never reach
    /// the cascade.
    fn read_css_with_imports(
        &self,
        path: &Path,
        visited: &mut std::collections::HashSet<String>,
    ) -> Option<String> {
        let key = path.to_string_lossy().replace('\\', "/");
        if !visited.insert(key) {
            return Some(String::new()); // import cycle, treat as empty
        }
        let bytes = self.load_asset_immutable(path).ok()?;
        let raw = String::from_utf8_lossy(&bytes).into_owned();
        Some(inline_css_imports(&raw, path, |child_path| {
            self.read_css_with_imports(child_path, visited)
        }))
    }

    /// Like load_asset, but takes &self (not &mut self) so it can be used
    /// from within the recursive @import resolver. The EPUB asset reader
    /// already only needs immutable state.
    fn load_asset_immutable(&self, path: &Path) -> io::Result<Vec<u8>> {
        let key = path.to_string_lossy().replace('\\', "/");
        self.read_entry(&key)
    }
}

/// Replace each `@import` directive with the contents of the referenced
/// file, resolved relative to `base`. Handles all three syntaxes the CSS
/// spec defines:
/// - `@import "url";` / `@import 'url';` (quoted)
/// - `@import url("url");` / `url('url')` / `url(url)` (function form)
///
/// Japanese EPUBs converted from AZW3 commonly use `url(...)` to chain
/// stylesheets (style0012.css imports style0010.css where `.vrtl` lives);
/// without resolving these the `writing-mode: vertical-rl` rule never reaches
/// the cascade and the KFX exporter falls back to horizontal_tb.
fn inline_css_imports<F>(src: &str, base: &Path, mut load: F) -> String
where
    F: FnMut(&Path) -> Option<String>,
{
    let mut out = String::with_capacity(src.len());
    let bytes = src.as_bytes();
    // Index of the first byte not yet copied into `out`. Byte scans are safe
    // because every token we look for (@, " ', ;, whitespace, parens) is
    // ASCII and therefore never appears as a UTF-8 continuation byte — so
    // `i` always lands on a char boundary when we slice.
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' && src[i..].to_ascii_lowercase().starts_with("@import") {
            let after_kw = i + "@import".len();
            let mut j = after_kw;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            // Try to parse one of the supported source-URL forms. Each
            // helper returns `(url, end_index_past_url_token)` on success.
            let parsed = if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
                parse_quoted_url(src, j)
            } else if j + 4 <= bytes.len() && src[j..j + 4].eq_ignore_ascii_case("url(") {
                parse_url_function(src, j)
            } else {
                None
            };
            if let Some((url, mut k)) = parsed {
                // Skip optional media queries / whitespace up to the `;`.
                while k < bytes.len() && bytes[k] != b';' && bytes[k] != b'}' {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == b';' {
                    k += 1;
                }
                // Copy everything before the @import as-is, then splice in
                // the imported file (or drop the @import on load failure).
                out.push_str(&src[copied..i]);
                // PathBuf::join doesn't collapse `..`, so a chained import
                // like `style0011.css` → `url("../Styles/style0007.css")`
                // yields `OEBPS/Styles/../Styles/style0007.css` and silently
                // misses the canonical zip entry. Normalize before loading.
                // `url` is a URI reference; decode it so the child path matches
                // the literal zip entry name.
                let url = percent_decode(url);
                let joined = base
                    .parent()
                    .map(|p| p.join(&url))
                    .unwrap_or_else(|| PathBuf::from(&url));
                let child = normalize_components(&joined);
                if let Some(child_css) = load(&child) {
                    out.push_str(&child_css);
                    out.push('\n');
                }
                copied = k;
                i = k;
                continue;
            }
            // Unrecognised @import form — fall through and keep scanning.
        }
        i += 1;
    }
    out.push_str(&src[copied..]);
    out
}

/// Parse a `"url"` / `'url'` literal starting at the opening quote position.
/// Returns `(url, index_one_past_closing_quote)`.
fn parse_quoted_url(src: &str, q_pos: usize) -> Option<(&str, usize)> {
    let bytes = src.as_bytes();
    let quote = bytes[q_pos];
    let start = q_pos + 1;
    let end_rel = src.as_bytes()[start..].iter().position(|&b| b == quote)?;
    Some((&src[start..start + end_rel], start + end_rel + 1))
}

/// Parse a `url( … )` token starting at the `u` of `url`. Inner content can be
/// `"foo.css"`, `'foo.css'`, or a bare `foo.css`. Returns
/// `(url, index_one_past_closing_paren)`.
fn parse_url_function(src: &str, u_pos: usize) -> Option<(&str, usize)> {
    let bytes = src.as_bytes();
    let mut p = u_pos + 4; // skip "url("
    while p < bytes.len() && bytes[p].is_ascii_whitespace() {
        p += 1;
    }
    let (url, end) = if p < bytes.len() && (bytes[p] == b'"' || bytes[p] == b'\'') {
        parse_quoted_url(src, p)?
    } else {
        // Bare URL: read until whitespace or `)`
        let url_start = p;
        while p < bytes.len() && bytes[p] != b')' && !bytes[p].is_ascii_whitespace() {
            p += 1;
        }
        (&src[url_start..p], p)
    };
    let mut k = end;
    while k < bytes.len() && bytes[k].is_ascii_whitespace() {
        k += 1;
    }
    if k >= bytes.len() || bytes[k] != b')' {
        return None;
    }
    Some((url, k + 1))
}

// ----------------------------------------------------------------------------
// ZIP IO Helpers
// ----------------------------------------------------------------------------

fn read_entry(
    source: &Arc<dyn ByteSource>,
    index: &HashMap<String, ZipEntryLoc>,
    path: &str,
) -> io::Result<Vec<u8>> {
    let loc = index.get(path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("File not found in ZIP: {}", path),
        )
    })?;

    // Read compressed data via random access
    let compressed = source.read_at(loc.data_offset, loc.compressed_size as usize)?;

    // Decompress
    match loc.compression {
        0 => Ok(compressed), // Stored
        8 => {
            // Deflate
            let mut decoder = flate2::read::DeflateDecoder::new(&compressed[..]);
            let cap = usize::try_from(loc.uncompressed_size).unwrap_or(0);
            let mut out = Vec::with_capacity(cap);
            decoder.read_to_end(&mut out)?;
            Ok(out)
        }
        method => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("Unsupported compression method: {}", method),
        )),
    }
}

fn compression_to_u16(method: zip::CompressionMethod) -> u16 {
    match method {
        zip::CompressionMethod::Stored => 0,
        zip::CompressionMethod::Deflated => 8,
        _ => 255,
    }
}

/// Upper bound (in bytes) on the heading text indexed for TOC repair. Long
/// enough for any real chapter title, short enough that an id sitting on a
/// chapter-sized wrapper is rejected without walking its whole subtree.
const HEADING_TEXT_BYTE_CAP: usize = 400;

/// Concatenate the text of the subtree rooted at `node`, in document order,
/// stopping once `cap` bytes have been collected.
fn collect_node_text(chapter: &Chapter, node: NodeId, cap: usize) -> String {
    let mut out = String::new();
    collect_subtree_text(chapter, node, cap, &mut out);
    out
}

fn collect_subtree_text(chapter: &Chapter, node: NodeId, cap: usize, out: &mut String) {
    if out.len() >= cap {
        return;
    }
    let Some(n) = chapter.node(node) else {
        return;
    };
    if n.role == Role::Text {
        out.push_str(chapter.text(n.text));
    }
    let mut child = n.first_child;
    while let Some(c) = child {
        if out.len() >= cap {
            break;
        }
        collect_subtree_text(chapter, c, cap, out);
        child = chapter.node(c).and_then(|cn| cn.next_sibling);
    }
}

/// Strip every Unicode whitespace char (including the ideographic space U+3000
/// these EPUBs put between a chapter number and its title) so a TOC label and a
/// heading that differ only in spacing compare equal.
fn strip_whitespace(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Repair fragment-less TOC entries in place by matching each entry's label to
/// a unique id-bearing element in its target file. Entries that already carry a
/// `#fragment`, that have no matching heading, or whose label matches more than
/// one heading are left untouched. See [`EpubImporter::resolve_toc`].
fn repair_flat_toc_fragments(
    entries: &mut [TocEntry],
    heading_ids: &HashMap<String, Vec<(String, String)>>,
) {
    for entry in entries {
        if !entry.href.is_empty()
            && !entry.href.contains('#')
            && let Some(candidates) = heading_ids.get(entry.href.as_str())
        {
            let needle = strip_whitespace(&entry.title);
            if !needle.is_empty() {
                let mut matched: Option<&str> = None;
                let mut ambiguous = false;
                for (text, id) in candidates {
                    if *text == needle {
                        if matched.is_some() {
                            ambiguous = true;
                            break;
                        }
                        matched = Some(id);
                    }
                }
                if let Some(id) = matched
                    && !ambiguous
                {
                    entry.href = format!("{}#{}", entry.href, id);
                }
            }
        }
        repair_flat_toc_fragments(&mut entry.children, heading_ids);
    }
}

/// Total entries in a TOC tree, counting nested children. Used to pick the
/// richer of a book's NCX vs nav-doc TOC when it ships both (some retail EPUBs
/// pair a full nav with a stub NCX, or vice versa).
fn count_toc_entries(entries: &[TocEntry]) -> usize {
    entries
        .iter()
        .map(|e| 1 + count_toc_entries(&e.children))
        .sum()
}

/// Directory portion of an archive path, with a trailing `/` (empty string when
/// the path sits at the archive root). Archive entry names are always
/// `/`-delimited, so this splits on `/` rather than going through `Path` (whose
/// separator is platform-dependent). Used to resolve a document's relative
/// hrefs against the directory that document lives in.
fn archive_dir_base(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/"),
        None => String::new(),
    }
}

/// Prepend base path to TOC entry hrefs (NCX uses relative paths).
///
/// TOC hrefs are URI references, so they are percent-decoded here to match the
/// decoded chapter paths and anchor-map keys they resolve against.
fn prepend_base_to_toc(entries: &[TocEntry], base: &str) -> Vec<TocEntry> {
    entries
        .iter()
        .map(|entry| {
            let decoded = percent_decode(&entry.href);
            let href = if decoded.starts_with('#') || decoded.is_empty() {
                decoded
            } else {
                format!("{}{}", base, decoded)
            };
            TocEntry {
                title: entry.title.clone(),
                href,
                children: prepend_base_to_toc(&entry.children, base),
                play_order: entry.play_order,
                target: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_archive_dir_base() {
        // Root-level document → empty base.
        assert_eq!(archive_dir_base("9781668011799.opf"), "");
        assert_eq!(archive_dir_base("toc.ncx"), "");
        // Subdirectory document → its directory, trailing slash.
        assert_eq!(
            archive_dir_base("e9781668011799/xhtml/nav.xhtml"),
            "e9781668011799/xhtml/"
        );
        assert_eq!(archive_dir_base("OEBPS/content.opf"), "OEBPS/");
    }

    #[test]
    fn test_toc_base_is_document_dir_not_opf_dir() {
        // Regression: a nav doc in a subdirectory (OPF+NCX at the archive root,
        // nav at `xhtml/nav.xhtml`) resolves its fragment-less chapter hrefs
        // against the nav doc's own directory, not the OPF's. Prepending the
        // OPF base (empty here) once dropped every chapter from the KFX TOC.
        let nav_path = "e9781668011799/xhtml/nav.xhtml";
        let doc_base = archive_dir_base(nav_path);
        let entries = vec![
            TocEntry::new("Chapter One", "ch01.xhtml"),
            TocEntry::new("Copyright", "copyright.xhtml"),
        ];
        let result = prepend_base_to_toc(&entries, &doc_base);
        assert_eq!(result[0].href, "e9781668011799/xhtml/ch01.xhtml");
        assert_eq!(result[1].href, "e9781668011799/xhtml/copyright.xhtml");
    }

    #[test]
    fn test_prepend_base_to_toc_simple() {
        let entries = vec![
            TocEntry::new("Chapter 1", "text/ch1.xhtml"),
            TocEntry::new("Chapter 2", "text/ch2.xhtml"),
        ];

        let result = prepend_base_to_toc(&entries, "OEBPS/");

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].href, "OEBPS/text/ch1.xhtml");
        assert_eq!(result[1].href, "OEBPS/text/ch2.xhtml");
    }

    #[test]
    fn test_prepend_base_to_toc_with_fragments() {
        let entries = vec![
            TocEntry::new("Section 1", "text/ch1.xhtml#section1"),
            TocEntry::new("Section 2", "text/ch1.xhtml#section2"),
        ];

        let result = prepend_base_to_toc(&entries, "epub/");

        assert_eq!(result[0].href, "epub/text/ch1.xhtml#section1");
        assert_eq!(result[1].href, "epub/text/ch1.xhtml#section2");
    }

    #[test]
    fn test_prepend_base_to_toc_preserves_anchor_only() {
        let entries = vec![
            TocEntry::new("Internal Link", "#footnote1"),
            TocEntry::new("Empty", ""),
        ];

        let result = prepend_base_to_toc(&entries, "OEBPS/");

        // Anchor-only hrefs should not be modified
        assert_eq!(result[0].href, "#footnote1");
        // Empty hrefs should not be modified
        assert_eq!(result[1].href, "");
    }

    #[test]
    fn test_prepend_base_to_toc_nested() {
        let mut parent = TocEntry::new("Part I", "text/part1.xhtml");
        parent.children = vec![
            TocEntry::new("Chapter 1", "text/ch1.xhtml"),
            TocEntry::new("Chapter 2", "text/ch2.xhtml"),
        ];
        let entries = vec![parent];

        let result = prepend_base_to_toc(&entries, "epub/");

        assert_eq!(result[0].href, "epub/text/part1.xhtml");
        assert_eq!(result[0].children.len(), 2);
        assert_eq!(result[0].children[0].href, "epub/text/ch1.xhtml");
        assert_eq!(result[0].children[1].href, "epub/text/ch2.xhtml");
    }

    #[test]
    fn test_prepend_base_to_toc_deeply_nested() {
        let grandchild = TocEntry::new("Section", "text/ch1.xhtml#sec1");
        let mut child = TocEntry::new("Chapter 1", "text/ch1.xhtml");
        child.children = vec![grandchild];
        let mut parent = TocEntry::new("Part I", "text/part1.xhtml");
        parent.children = vec![child];
        let entries = vec![parent];

        let result = prepend_base_to_toc(&entries, "content/");

        assert_eq!(result[0].href, "content/text/part1.xhtml");
        assert_eq!(result[0].children[0].href, "content/text/ch1.xhtml");
        assert_eq!(
            result[0].children[0].children[0].href,
            "content/text/ch1.xhtml#sec1"
        );
    }

    #[test]
    fn inline_css_imports_normalizes_parent_dir_in_url() {
        // Mirrors the structure in books 2/5 of the
        // `epub2kfx-missing-vertical-outliers/涼, 結城/` set: a stylesheet
        // chains via `@import url("../Styles/x.css")`. The load callback must
        // see the canonical zip key, not the literal un-normalized path —
        // otherwise the writing-mode rules never reach the cascade and the
        // KFX exporter falls back to horizontal-tb.
        let base = std::path::Path::new("OEBPS/Styles/style0011.css");
        let mut requested: Vec<String> = Vec::new();
        let out = inline_css_imports(
            r#"@import url("../Styles/style0007.css"); body {}"#,
            base,
            |p| {
                requested.push(p.to_string_lossy().into_owned());
                Some(".vrtl { writing-mode: vertical-rl }".into())
            },
        );
        assert_eq!(requested, vec!["OEBPS/Styles/style0007.css"]);
        assert!(out.contains(".vrtl"));
    }

    #[test]
    fn inline_css_imports_handles_bare_url_function() {
        // Books 3/4 form: `@import url(file.css)` with no `..` — must keep
        // working after the normalization patch.
        let base = std::path::Path::new("OEBPS/Styles/flow0011.css");
        let mut requested: Vec<String> = Vec::new();
        let _ = inline_css_imports(r#"@import url(flow0007.css);"#, base, |p| {
            requested.push(p.to_string_lossy().into_owned());
            Some(String::new())
        });
        assert_eq!(requested, vec!["OEBPS/Styles/flow0007.css"]);
    }
}
