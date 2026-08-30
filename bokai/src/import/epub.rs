//! EPUB format importer - handles all IO.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

use crate::formats::epub::{
    parse_container_xml, parse_nav_landmarks, parse_nav_page_list, parse_nav_toc, parse_ncx,
    parse_opf, parse_opf_guide,
    structure::{dir_of, resolve_href},
};
use crate::html::Stylesheet;
use crate::import::{
    ChapterId, Importer, SpineEntry, normalize_components, resolve_path_based_href, viewport_meta,
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
    path_to_chapter: HashMap<String, ChapterId>,

    /// Maps "path#id" -> GlobalNodeId for fragment resolution
    anchor_map: HashMap<String, GlobalNodeId>,

    /// Chapter path → [(whitespace-stripped heading text, element id)] for every
    /// short id-bearing element. [`resolve_toc`] repairs flat TOCs whose hrefs
    /// dropped the `#fragment` against it.
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

                    // Index the id-bearing element by its (short) heading text:
                    // the key a fragment-less TOC href is repaired against.
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

    /// Repair flat TOCs: calibre and some retail EPUBs collapse several headings
    /// into one file and emit a `#fragment`-less href for each. Match every such
    /// entry's label to a unique id-bearing element in its target file.
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
    /// Shared href resolver. `chapter_fallback` true lands a dead
    /// `path#fragment` at the chapter start (navigation), false returns `None`.
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
    /// [`from_source`] retries it against a repaired in-memory copy when the raw
    /// bytes trip the `zip` crate.
    fn scan_zip(
        source: &Arc<dyn ByteSource>,
    ) -> io::Result<(HashMap<String, ZipEntryLoc>, Vec<PathBuf>)> {
        let cursor = ByteSourceCursor::new(source.clone());
        let mut archive = ZipArchive::new(cursor)?;

        let mut zip_index = HashMap::new();
        let mut assets = Vec::new();

        for i in 0..archive.len() {
            let file = archive.by_index(i)?;

            // Skip ZIP directory entries (names ending in `/`): a directory is
            // not a resource, and one enumerated as an asset lands a bogus
            // `href="OEBPS/"` manifest item (epubcheck RSC-001).
            if file.is_dir() {
                continue;
            }
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
        // 1. Scan the ZIP central directory and cache entry locations. A failed
        //    scan retries once on a copy repaired by
        //    `epub::neutralize_spurious_zip64`.
        let (zip_index, assets, source) = match Self::scan_zip(&source) {
            Ok((zip_index, assets)) => (zip_index, assets, source),
            Err(first_err) => {
                let raw = source.read_at(0, source.len() as usize)?;
                match crate::formats::epub::neutralize_spurious_zip64(&raw) {
                    Some(repaired) => {
                        let repaired: Arc<dyn ByteSource> = Arc::new(MemorySource::new(repaired));
                        let (zip_index, assets) = Self::scan_zip(&repaired)?;
                        (zip_index, assets, repaired)
                    }
                    None => return Err(first_err),
                }
            }
        };

        // 2. Find the OPF path from container.xml. The `full-path` is a URI
        //    reference: percent-decode it to the literal zip entry name.
        let container_bytes = read_entry(&source, &zip_index, "META-INF/container.xml")?;
        let opf_path = percent_decode(&parse_container_xml(&container_bytes)?);
        let opf_base = dir_of(&opf_path);

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
                // Manifest hrefs are URI references relative to the OPF's
                // directory. `resolve_href` decodes them and collapses `.`/`..`
                // to the literal zip entry name.
                let full_path = resolve_href(&opf_base, href);
                let size_estimate = zip_index
                    .get(&full_path)
                    .map(|loc| loc.compressed_size as usize)
                    .unwrap_or(0);

                // A document's own `<meta name="viewport">` states the pixel box
                // it is drawn to — a full-page illustration or spread carries one
                // whether or not the package declares `rendition:layout`.
                let page = read_entry(&source, &zip_index, &full_path)
                    .ok()
                    .map(|b| String::from_utf8_lossy(&b).into_owned());
                let viewport = page.as_deref().and_then(viewport_meta);

                // A comic's panels state their rectangles in the page's own
                // `<link>`ed sheets, whose `top` / `left` the style model does
                // not carry. The geometry comes off the CSS text.
                let panels = match (&page, viewport.or(opf.metadata.default_viewport)) {
                    (Some(html), Some(box_px)) if html.contains("app-amzn-magnify") => {
                        let (linked, inline) = crate::html::extract_stylesheets(html);
                        let mut sheets: Vec<String> = linked
                            .into_iter()
                            .map(|href| resolve_href(&dir_of(&full_path), &href))
                            .filter_map(|path| read_entry(&source, &zip_index, &path).ok())
                            .map(|b| String::from_utf8_lossy(&b).into_owned())
                            .collect();
                        sheets.extend(inline);
                        crate::html::parse_panels(html, &sheets, box_px)
                    }
                    _ => Vec::new(),
                };

                spine.push(SpineEntry {
                    id: ChapterId(i as u32),
                    size_estimate,
                    page_spread: opf
                        .spine_properties
                        .get(spine_id)
                        .and_then(|p| crate::model::PageSpread::from_opf_properties(p)),
                    viewport,
                    panels,
                });
                spine_paths.push(full_path);
            }
        }

        // Assets = non-spine resources (images / CSS / fonts / audio): every
        // `scan_zip` entry less the container structure (mimetype, META-INF/*,
        // the OPF), the regenerated NCX and nav doc, and every spine chapter.
        let mut assets = assets;
        {
            let spine_set: std::collections::HashSet<&str> =
                spine_paths.iter().map(|s| s.as_str()).collect();
            let ncx_path = opf.ncx_href.as_ref().map(|h| resolve_href(&opf_base, h));
            let nav_path = opf.nav_href.as_ref().map(|h| resolve_href(&opf_base, h));
            assets.retain(|p| {
                // `&str` bound explicitly: another `AsRef`/`Borrow` impl for
                // `Cow<str>` in the dependency graph makes an inferred call
                // ambiguous.
                let name: &str = &p.to_string_lossy();
                name != "mimetype"
                    && !name.starts_with("META-INF/")
                    && name != opf_path
                    && Some(name) != ncx_path.as_deref()
                    && Some(name) != nav_path.as_deref()
                    && !spine_set.contains(name)
            });
        }

        // 5. Parse the TOC from both the EPUB 3 nav doc and the EPUB 2 NCX —
        // retail Japanese EPUBs (Kadokawa/EBPAJ) ship a full nav doc beside a
        // stub NCX. The richer of the two wins, the nav doc taking a tie.
        let read_toc = |href: Option<&String>, parse: fn(&str) -> io::Result<Vec<TocEntry>>| {
            let href = href?;
            let path = resolve_href(&opf_base, href);
            let bytes = read_entry(&source, &zip_index, &path).ok()?;
            let hint_encoding = crate::util::extract_xml_encoding(&bytes);
            let text = crate::util::decode_text(&bytes, hint_encoding);
            let entries = parse(&text).ok()?;
            // Hrefs in a nav doc / NCX are relative to THAT document's
            // directory, not the OPF's; the two coincide only where both
            // documents sit in one directory.
            let doc_base = dir_of(&path);
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
        // the same EPUB 3 nav doc, base-prefixed exactly like the TOC. Amazon
        // carries it as a `page_list` nav_container.
        let page_list = read_toc(opf.nav_href.as_ref(), parse_nav_page_list).unwrap_or_default();

        // 6. Parse landmarks from EPUB 3 nav document
        let mut landmarks = if let Some(nav_href) = &opf.nav_href {
            let nav_path = resolve_href(&opf_base, nav_href);
            // Landmark hrefs are relative to the nav doc's directory, not the
            // OPF's (see the TOC note above).
            let nav_base = dir_of(&nav_path);
            if let Ok(nav_bytes) = read_entry(&source, &zip_index, &nav_path) {
                let hint_encoding = crate::util::extract_xml_encoding(&nav_bytes);
                let nav_str = crate::util::decode_text(&nav_bytes, hint_encoding);
                let mut parsed = parse_nav_landmarks(&nav_str)?;
                // Resolve against the nav doc's directory: the targets match
                // decoded chapter paths.
                for landmark in &mut parsed {
                    if !landmark.href.starts_with('#') && !landmark.href.is_empty() {
                        landmark.href = resolve_href(&nav_base, &landmark.href);
                    } else {
                        landmark.href = percent_decode(&landmark.href);
                    }
                }
                parsed
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        // 6b. Merge EPUB 2.0 `<guide>` landmarks into the nav doc's. Missing
        // types join the union in both directions: a nav doc omitting an
        // EPUB-2-only landmark keeps it, and the reverse holds.
        if let Ok(mut guide_marks) = parse_opf_guide(&opf_str) {
            for landmark in &mut guide_marks {
                if !landmark.href.starts_with('#') && !landmark.href.is_empty() {
                    landmark.href = resolve_href(&opf_base, &landmark.href);
                } else {
                    landmark.href = percent_decode(&landmark.href);
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

        // Resolve `cover_image` to an absolute (zip-relative) path matching the
        // asset keys downstream. The OPF parser leaves it as a manifest href
        // relative to `opf_base`.
        let mut metadata = opf.metadata;
        if let Some(ref href) = metadata.cover_image
            && !href.is_empty()
        {
            let path = resolve_href(&opf_base, href);
            let wrapped = svg_wrapped_image(&source, &zip_index, &path)
                .filter(|inner| zip_index.contains_key(inner));
            metadata.cover_image = Some(wrapped.unwrap_or(path));
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

    /// Read a CSS file and inline its `@import` rules into one flat stylesheet.
    /// The CSS parser skips at-rules other than `@font-face`; an un-inlined
    /// `@import`'s rules never reach the cascade.
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

    /// `load_asset` over `&self`, for the recursive `@import` resolver. The
    /// EPUB asset reader reads immutable state only.
    fn load_asset_immutable(&self, path: &Path) -> io::Result<Vec<u8>> {
        let key = path.to_string_lossy().replace('\\', "/");
        self.read_entry(&key)
    }
}

/// Replace each `@import` directive with the contents of the file it names,
/// resolved relative to `base`. Covers all three CSS syntaxes: `@import "url";`,
/// `@import 'url';`, and `@import url(...)` quoted or bare.
fn inline_css_imports<F>(src: &str, base: &Path, mut load: F) -> String
where
    F: FnMut(&Path) -> Option<String>,
{
    let mut out = String::with_capacity(src.len());
    let bytes = src.as_bytes();
    // Index of the first byte not yet copied into `out`. Every scanned token
    // (@, " ', ;, whitespace, parens) is ASCII and never appears as a UTF-8
    // continuation byte, keeping `i` on a char boundary.
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
                // `PathBuf::join` leaves `..` in place and `url` is a URI
                // reference: decode and normalize before loading, to reach the
                // canonical zip entry name.
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

/// The picture an SVG at `path` frames: the `href` of its one `<image>`,
/// resolved against the SVG's own directory. `None` for anything that is not
/// an SVG holding a single image. A manifest names both the frame and this.
fn svg_wrapped_image(
    source: &Arc<dyn ByteSource>,
    index: &HashMap<String, ZipEntryLoc>,
    path: &str,
) -> Option<String> {
    if !path.to_ascii_lowercase().ends_with(".svg") {
        return None;
    }
    let bytes = read_entry(source, index, path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let base = match path.rfind('/') {
        Some(cut) => &path[..cut],
        None => "",
    };

    let mut reader = Reader::from_str(&text);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut href: Option<String> = None;
    loop {
        let event = reader.read_event_into(&mut buf);
        let (Ok(Event::Start(e)) | Ok(Event::Empty(e))) = event else {
            match event {
                Ok(Event::Eof) | Err(_) => break,
                _ => {
                    buf.clear();
                    continue;
                }
            }
        };
        if local_name_of(e.name().as_ref()) == b"image" {
            if href.is_some() {
                return None;
            }
            href = e.attributes().flatten().find_map(|a| {
                (local_name_of(a.key.as_ref()) == b"href")
                    .then(|| String::from_utf8_lossy(&a.value).into_owned())
            });
        }
        buf.clear();
    }
    let href = href?;
    (!href.is_empty() && !href.contains("://")).then(|| resolve_href(base, &href))
}

/// A qualified XML name without its namespace prefix.
fn local_name_of(name: &[u8]) -> &[u8] {
    match name.iter().position(|&b| b == b':') {
        Some(cut) => &name[cut + 1..],
        None => name,
    }
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

/// Strip every Unicode whitespace char, the ideographic space U+3000 included.
/// A TOC label and a heading differing only in spacing then compare equal.
fn strip_whitespace(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Repair fragment-less TOC entries in place, matching each entry's label to a
/// unique id-bearing element in its target file. An entry carrying a
/// `#fragment`, or matching no heading or several, stays untouched.
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

/// Total entries in a TOC tree, counting nested children. Picks the richer of a
/// book's NCX vs nav-doc TOC when it ships both.
fn count_toc_entries(entries: &[TocEntry]) -> usize {
    entries
        .iter()
        .map(|e| 1 + count_toc_entries(&e.children))
        .sum()
}

/// Resolve TOC entry hrefs against `base`. A URI reference is percent-decoded
/// and its `.`/`..` segments collapsed, matching the chapter paths and
/// anchor-map keys; an anchor-only href stays as it is.
fn prepend_base_to_toc(entries: &[TocEntry], base: &str) -> Vec<TocEntry> {
    entries
        .iter()
        .map(|entry| {
            let decoded = percent_decode(&entry.href);
            let href = if decoded.starts_with('#') || decoded.is_empty() {
                decoded
            } else {
                resolve_href(base, &entry.href)
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

    /// An EPUB whose declared cover is an SVG framing `image/pic.jpg`, the
    /// shape a converter writes for a cover page. `trailing` appends bytes past
    /// the closing tag, which no XML parser accepts.
    fn epub_with_svg_cover(trailing: &[u8]) -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opt = SimpleFileOptions::default();
            let mut put = |name: &str, body: &[u8]| {
                zip.start_file(name, opt).unwrap();
                zip.write_all(body).unwrap();
            };
            put("mimetype", b"application/epub+zip");
            put(
                "META-INF/container.xml",
                br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            );
            put(
                "OEBPS/content.opf",
                br#"<?xml version="1.0" encoding="utf-8"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="uid">x</dc:identifier><dc:title>t</dc:title><dc:language>en</dc:language></metadata><manifest><item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/><item id="cov" href="cover.svg" media-type="image/svg+xml" properties="cover-image"/><item id="pic" href="image/pic.jpg" media-type="image/jpeg"/></manifest><spine><itemref idref="c1"/></spine></package>"#,
            );
            put(
                "OEBPS/ch1.xhtml",
                br#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>c1</title></head><body><p>one</p></body></html>"#,
            );
            let mut svg = br#"<?xml version="1.0" encoding="UTF-8"?><svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 640 920"><image xlink:href="image/pic.jpg" width="640" height="920"/></svg>"#.to_vec();
            svg.extend_from_slice(trailing);
            put("OEBPS/cover.svg", &svg);
            put("OEBPS/image/pic.jpg", b"\xFF\xD8\xFFnot-really-a-jpeg");
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    /// The cover of a book whose manifest names an SVG frame is the picture
    /// inside it: KFX and PDF carry no vector resource format, and the frame
    /// reaches a device as a resource that draws nothing.
    #[test]
    fn a_cover_svg_resolves_to_the_picture_it_frames() {
        for trailing in [b"".as_slice(), b"\x88\x99\x26\xd6".as_slice()] {
            let bytes = epub_with_svg_cover(trailing);
            let importer =
                EpubImporter::from_source(Arc::new(MemorySource::new(bytes))).expect("opens");
            assert_eq!(
                importer.metadata().cover_image.as_deref(),
                Some("OEBPS/image/pic.jpg"),
                "trailing bytes: {}",
                trailing.len()
            );
        }
    }

    /// A minimal EPUB (mimetype, container, OPF, one spine doc) plus an
    /// explicit `OEBPS/` directory entry.
    fn epub_with_directory_entry() -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opt = SimpleFileOptions::default();
            zip.start_file("mimetype", opt).unwrap();
            zip.write_all(b"application/epub+zip").unwrap();
            zip.start_file("META-INF/container.xml", opt).unwrap();
            zip.write_all(
                br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            )
            .unwrap();
            // An explicit directory entry: a zip member with no bytes, which
            // must not be collected as an asset.
            zip.add_directory("OEBPS", opt).unwrap();
            zip.start_file("OEBPS/content.opf", opt).unwrap();
            zip.write_all(
                br#"<?xml version="1.0" encoding="utf-8"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="uid">x</dc:identifier><dc:title>t</dc:title><dc:language>en</dc:language></metadata><manifest><item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c1"/></spine></package>"#,
            )
            .unwrap();
            zip.start_file("OEBPS/ch1.xhtml", opt).unwrap();
            zip.write_all(
                br#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>c1</title></head><body><p>x</p></body></html>"#,
            )
            .unwrap();
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    /// A Sigil-authored EPUB whose OPF sits in `OEBPS/` but keeps a spine doc,
    /// the nav, and the NCX at the archive root, referencing each with a `../`
    /// href. Real books ship this shape.
    fn epub_with_parent_escaping_hrefs() -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opt = SimpleFileOptions::default();
            let mut put = |name: &str, body: &[u8]| {
                zip.start_file(name, opt).unwrap();
                zip.write_all(body).unwrap();
            };
            put("mimetype", b"application/epub+zip");
            put(
                "META-INF/container.xml",
                br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            );
            put(
                "OEBPS/content.opf",
                br#"<?xml version="1.0" encoding="utf-8"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="uid">x</dc:identifier><dc:title>t</dc:title><dc:language>en</dc:language></metadata><manifest><item id="c1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/><item id="c2" href="../ch2.xhtml" media-type="application/xhtml+xml"/><item id="nav" href="../nav.xhtml" media-type="application/xhtml+xml" properties="nav"/><item id="ncx" href="../toc.ncx" media-type="application/x-dtbncx+xml"/><item id="cov" href="../cover.jpg" media-type="image/jpeg" properties="cover-image"/></manifest><spine toc="ncx"><itemref idref="c1"/><itemref idref="c2"/></spine></package>"#,
            );
            put(
                "OEBPS/Text/ch1.xhtml",
                br#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>c1</title></head><body><p>one</p></body></html>"#,
            );
            put(
                "ch2.xhtml",
                br#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>c2</title></head><body><p>two</p></body></html>"#,
            );
            // Root-level nav: its own hrefs are written relative to the root,
            // resolving against the nav's directory, not the OPF's.
            put(
                "nav.xhtml",
                br#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="OEBPS/Text/ch1.xhtml">One</a></li><li><a href="ch2.xhtml">Two</a></li></ol></nav></body></html>"#,
            );
            put(
                "toc.ncx",
                br#"<?xml version="1.0" encoding="utf-8"?><ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1"><navMap><navPoint id="n1" playOrder="1"><navLabel><text>One</text></navLabel><content src="OEBPS/Text/ch1.xhtml"/></navPoint></navMap></ncx>"#,
            );
            put("cover.jpg", b"\xFF\xD8\xFFnot-really-a-jpeg");
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn manifest_hrefs_that_escape_the_opf_directory_resolve() {
        // Joining `opf_base` to the href by concatenation produces
        // `OEBPS/../ch2.xhtml`, which names no zip entry and kills the whole
        // conversion with "File not found in ZIP". `..` has to be collapsed.
        let bytes = epub_with_parent_escaping_hrefs();
        let mut importer =
            EpubImporter::from_source(Arc::new(MemorySource::new(bytes))).expect("opens");

        assert_eq!(importer.spine().len(), 2);
        assert_eq!(
            importer.source_id(ChapterId(0)),
            Some("OEBPS/Text/ch1.xhtml")
        );
        assert_eq!(importer.source_id(ChapterId(1)), Some("ch2.xhtml"));
        // Every spine doc must actually read back — the failure mode was a path
        // that resolved to nothing.
        for id in 0..2u32 {
            assert!(
                importer.load_raw(ChapterId(id)).is_ok(),
                "spine doc {id} unreadable"
            );
        }
        // The nav doc lives at the root; its own hrefs resolve against the
        // root, not against `OEBPS/`.
        let toc = importer.toc();
        assert_eq!(toc.len(), 2, "nav doc at `../nav.xhtml` was not read");
        assert_eq!(toc[0].href, "OEBPS/Text/ch1.xhtml");
        assert_eq!(toc[1].href, "ch2.xhtml");
        assert_eq!(
            importer.metadata().cover_image.as_deref(),
            Some("cover.jpg")
        );
    }

    #[test]
    fn directory_entries_are_not_treated_as_assets() {
        let bytes = epub_with_directory_entry();
        let importer = EpubImporter::from_source(Arc::new(MemorySource::new(bytes))).unwrap();
        // The bare `OEBPS/` directory must not appear as an asset — enumerating
        // it produced a bogus `href="OEBPS/"` manifest item (RSC-001) on export.
        for asset in importer.list_assets() {
            let name = asset.to_string_lossy();
            assert!(
                !name.ends_with('/'),
                "directory entry leaked into assets: {name:?}"
            );
        }
        // The one real content doc is the spine chapter (not an asset), leaving
        // the asset list empty: the directory was the only other entry.
        assert!(
            importer.list_assets().is_empty(),
            "only entries were the spine doc + a directory; got assets: {:?}",
            importer.list_assets()
        );
    }

    #[test]
    fn test_dir_of() {
        // Root-level document → empty base.
        assert_eq!(dir_of("9781668011799.opf"), "");
        assert_eq!(dir_of("toc.ncx"), "");
        // Subdirectory document → its directory, trailing slash.
        assert_eq!(
            dir_of("e9781668011799/xhtml/nav.xhtml"),
            "e9781668011799/xhtml/"
        );
        assert_eq!(dir_of("OEBPS/content.opf"), "OEBPS/");
    }

    #[test]
    fn test_toc_base_is_document_dir_not_opf_dir() {
        // A nav doc in a subdirectory (OPF+NCX at the archive root, nav at
        // `xhtml/nav.xhtml`) resolves its fragment-less chapter hrefs against
        // the nav doc's own directory, not the OPF's.
        let nav_path = "e9781668011799/xhtml/nav.xhtml";
        let doc_base = dir_of(nav_path);
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
        // A stylesheet chains via `@import url("../Styles/x.css")`. The load
        // callback sees the canonical zip key, not the literal un-normalized
        // path.
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

#[cfg(test)]
mod panel_tests {
    /// Panels survive the AZW3 → EPUB → KFX route: the exported EPUB names
    /// them, and the importer reads them back off the page's own sheets.
    #[test]
    #[ignore = "needs the EPUB fixtures under artifacts/"]
    fn panels_reach_the_spine_from_a_converted_epub() {
        let book = crate::Book::open(
            "../artifacts/graphicnovel-azw3/converted-epub/Tetris_ The Games People Play_B01M28OM76.epub",
        )
        .unwrap();
        let counts: Vec<usize> = book.spine().iter().map(|e| e.panels.len()).collect();
        assert_eq!(counts.iter().sum::<usize>(), 893);
        // The two densest spreads, matched by count.
        let mut densest = counts.clone();
        densest.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(&densest[..2], &[19, 18]);

        let page = book
            .spine()
            .iter()
            .find(|e| e.panels.len() == 19)
            .expect("the 19-panel spread");
        assert_eq!(page.panels[0].ordinal, 1);
        assert!(page.panels[0].image.width > 1.0);
    }
}
