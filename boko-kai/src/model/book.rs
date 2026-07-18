//! Core data types and runtime handle for ebooks.
//!
//! This module provides:
//! - Format-agnostic types (`Metadata`, `TocEntry`, `Resource`, `SpineItem`)
//! - The `Book` runtime handle for reading ebooks via importers

use std::collections::HashMap;
use std::io::{self, Seek, Write};
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::export::{EpubExporter, Exporter, KfxExporter, MarkdownExporter};
use crate::import::{
    Azw3Importer, ChapterId, EpubImporter, Importer, KfxImporter, MobiImporter, SpineEntry,
};
use crate::io::MemorySource;
use crate::model::resolved::resolve_book_links;
use crate::model::{AnchorTarget, Chapter, ResolvedLinks};

// ============================================================================
// Data Types
// ============================================================================

/// Ebook file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// EPUB format (EPUB 2 or 3)
    Epub,
    /// AZW3/KF8 format (modern Kindle, input-only)
    Azw3,
    /// MOBI format (legacy Kindle, input-only)
    Mobi,
    /// KFX format (Kindle Format 10)
    Kfx,
    /// Markdown (export only)
    Markdown,
}

/// A resource (image, font, CSS, etc.) with its data and media type.
#[derive(Debug, Clone)]
pub struct Resource {
    pub data: Vec<u8>,
    pub media_type: String,
}

/// A contributor with optional role and sort name.
#[derive(Debug, Clone, Default)]
pub struct Contributor {
    pub name: String,
    pub file_as: Option<String>,
    /// MARC relator code: "trl", "edt", "ill", etc.
    pub role: Option<String>,
}

/// Collection/series information.
#[derive(Debug, Clone)]
pub struct CollectionInfo {
    pub name: String,
    /// "series" or "set"
    pub collection_type: Option<String>,
    /// group-position (1, 2, 3.5, etc.)
    pub position: Option<f64>,
}

/// Book metadata (Dublin Core + extensions)
#[derive(Debug, Clone, Default)]
pub struct Metadata {
    pub title: String,
    pub authors: Vec<String>,
    pub language: String,
    pub identifier: String,
    /// Amazon Standard Identification Number, kept separate from `identifier`
    /// because KFX carries both a generic `book_id` (internal Kindle UUID)
    /// *and* an `ASIN` (catalogue id used by amazon.com / .co.jp /…). EPUB
    /// imports populate this from a `<dc:identifier opf:scheme="ASIN">` line
    /// when present.
    pub asin: Option<String>,
    pub publisher: Option<String>,
    pub description: Option<String>,
    pub subjects: Vec<String>,
    pub date: Option<String>,
    pub rights: Option<String>,
    pub cover_image: Option<String>,
    /// dcterms:modified timestamp
    pub modified_date: Option<String>,
    /// dc:contributor with roles (translators, editors, illustrators, etc.)
    pub contributors: Vec<Contributor>,
    /// file-as for title (sort key)
    pub title_sort: Option<String>,
    /// file-as for first author (sort key)
    pub author_sort: Option<String>,
    /// belongs-to-collection (series info)
    pub collection: Option<CollectionInfo>,
    /// OPF spine `page-progression-direction` attribute ("ltr" | "rtl" | "default").
    /// Required for vertical-RTL Japanese books to bind/swipe correctly on Kindle.
    pub page_progression_direction: Option<String>,
    /// OPF `<meta name="primary-writing-mode">` value (e.g. "vertical-rl",
    /// "vertical-lr"). Richer vocabulary than `page_progression_direction`:
    /// distinguishes Japanese vertical-rl from Mongolian vertical-lr, which
    /// share a `rtl` PPD but render differently.
    pub primary_writing_mode: Option<String>,
    /// Book is fixed-layout (image-based manga / comic, or a paginated
    /// picture book) — `rendition:layout = pre-paginated`. Drives FXL OPF
    /// metadata on EPUB export and the `yj_non_pdf_fixed_layout` skeleton on
    /// KFX export, instead of the reflowable path. When false the book flows.
    pub fixed_layout: bool,
    /// `book-type` OPF hint — `"comic"` for double-page-spread manga,
    /// `"children"` for picture books. Only meaningful when `fixed_layout`.
    pub book_type: Option<String>,
    /// `rendition:spread` policy for a fixed-layout book (e.g. `"landscape"`
    /// = show facing pages as a two-page spread in landscape). `None` leaves
    /// it to the reader default.
    pub rendition_spread: Option<String>,
    /// Book-level default page viewport `(width, height)` in px — the
    /// `fixed-layout-jp:viewport` / KF8 `original-resolution`. Every FXL page
    /// is authored to this box unless it carries its own viewport.
    pub default_viewport: Option<(u32, u32)>,
}

/// Which half of a two-page spread a fixed-layout page occupies — the OPF
/// spine itemref `page-spread-left` / `page-spread-right` /
/// `rendition:page-spread-center` property. Drives facing-page pairing in a
/// manga/comic; carries the source's declared pairing so it round-trips
/// losslessly rather than being re-derived by alternation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSpread {
    Left,
    Right,
    Center,
}

impl PageSpread {
    /// The OPF spine itemref property token.
    pub fn opf_property(self) -> &'static str {
        match self {
            PageSpread::Left => "page-spread-left",
            PageSpread::Right => "page-spread-right",
            PageSpread::Center => "rendition:page-spread-center",
        }
    }

    /// Parse from an OPF itemref `properties` attribute value.
    pub fn from_opf_properties(props: &str) -> Option<Self> {
        props.split_whitespace().find_map(|p| match p {
            "page-spread-left" | "rendition:page-spread-left" => Some(PageSpread::Left),
            "page-spread-right" | "rendition:page-spread-right" => Some(PageSpread::Right),
            "page-spread-center" | "rendition:page-spread-center" => Some(PageSpread::Center),
            _ => None,
        })
    }
}

/// A table of contents entry (hierarchical)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocEntry {
    pub title: String,
    pub href: String,
    pub children: Vec<TocEntry>,
    /// Play order for sorting (from NCX playOrder attribute)
    pub play_order: Option<usize>,
    /// Resolved target (set by `resolve_links()`)
    pub target: Option<AnchorTarget>,
}

impl Ord for TocEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.play_order.cmp(&other.play_order)
    }
}

impl PartialOrd for TocEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Type of landmark in a book's navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LandmarkType {
    /// Cover page (image)
    Cover,
    /// Title page
    TitlePage,
    /// Table of contents
    Toc,
    /// Start reading location (where the book opens)
    StartReading,
    /// Beginning of body/main content
    BodyMatter,
    /// Front matter (preface, introduction, etc.)
    FrontMatter,
    /// Back matter (appendix, index, etc.)
    BackMatter,
    /// Acknowledgements
    Acknowledgements,
    /// Bibliography
    Bibliography,
    /// Glossary
    Glossary,
    /// Index
    Index,
    /// Preface
    Preface,
    /// Endnotes/Footnotes
    Endnotes,
    /// List of illustrations
    Loi,
    /// List of tables
    Lot,
}

/// A landmark navigation entry.
///
/// Landmarks identify structural locations in a book (cover, start of content,
/// endnotes, etc.) used for navigation and reader features.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landmark {
    /// Type of landmark
    pub landmark_type: LandmarkType,
    /// Target href (file path with optional fragment)
    pub href: String,
    /// Display label
    pub label: String,
}

// ============================================================================
// Book Runtime Handle
// ============================================================================

/// Runtime handle for an ebook.
///
/// `Book` wraps a format-specific `Importer` backend and provides
/// unified access to metadata, table of contents, and content.
///
/// # Example
///
/// ```no_run
/// use boko::Book;
///
/// let mut book = Book::open("input.epub")?;
/// println!("Title: {}", book.metadata().title);
///
/// // Load chapter content (collect spine first to avoid borrow issues)
/// let spine: Vec<_> = book.spine().to_vec();
/// for entry in spine {
///     let raw = book.load_raw(entry.id)?;
///     println!("Chapter {}: {} bytes", entry.id.0, raw.len());
/// }
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct Book {
    backend: Box<dyn Importer>,
    /// Cache of parsed IR chapters to avoid re-parsing during normalized export.
    /// Uses RwLock for thread-safe access and Arc for cheap cloning.
    ir_cache: Arc<RwLock<HashMap<ChapterId, Arc<Chapter>>>>,
    /// Caller-supplied metadata that shadows the backend's parsed metadata.
    /// When set, [`Book::metadata`] returns this instead — so an exporter writes
    /// the override (e.g. Sidle's edited title/author from its library DB)
    /// without touching the source file. `None` = use the backend's metadata
    /// verbatim (the default).
    meta_override: Option<Metadata>,
    /// How raster images are encoded into a KFX export: `Color` (default —
    /// full `24bppRGB` JXR, for color devices like the Colorsoft and the Sidle
    /// desktop reader) or `Grayscale` (luma-only `8bppGray`). `Color` is safe as
    /// the default because the JXR encoder auto-collapses any image whose
    /// channels are identical (a grayscale source) to `8bppGray`, so only
    /// genuinely-color images carry chroma. Set via [`Book::set_image_color_mode`].
    image_color_mode: jxr::ColorMode,
}

impl Format {
    /// Detect format from file extension.
    pub fn from_path(path: impl AsRef<Path>) -> Option<Self> {
        path.as_ref()
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| match ext.to_lowercase().as_str() {
                "epub" => Some(Format::Epub),
                "azw3" => Some(Format::Azw3),
                "mobi" => Some(Format::Mobi),
                // `.kfx-zip` is Amazon's multi-container KFX bundle; the KFX
                // importer auto-detects and dispatches it via its `open()`.
                "kfx" | "kfx-zip" => Some(Format::Kfx),
                "md" | "txt" => Some(Format::Markdown),
                _ => None,
            })
    }

    /// Whether this format can be used for input/import.
    pub fn can_import(&self) -> bool {
        matches!(
            self,
            Format::Epub | Format::Azw3 | Format::Mobi | Format::Kfx
        )
    }

    /// Whether this format can be used for output/export.
    pub fn can_export(&self) -> bool {
        matches!(self, Format::Epub | Format::Kfx | Format::Markdown)
    }
}

impl Book {
    /// Open an ebook file, auto-detecting the format.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let format = Format::from_path(path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown file format: {}", path.display()),
            )
        })?;
        Self::open_format(path, format)
    }

    /// Open an ebook file with an explicit format.
    pub fn open_format(path: impl AsRef<Path>, format: Format) -> io::Result<Self> {
        let path = path.as_ref();
        let backend: Box<dyn Importer> = match format {
            Format::Epub => Box::new(EpubImporter::open(path)?),
            Format::Azw3 => Box::new(Azw3Importer::open(path)?),
            // `.mobi` covers three on-disk shapes: pure MOBI6, pure KF8, and
            // MOBI6+KF8 combo (older Amazon kindlegen output). The KF8-aware
            // shapes route through `Azw3Importer` so the source CSS, KF8
            // spine, and per-chapter `xml:lang` make it into the EPUB —
            // strict readers (Apple Books, KOReader) need that for vertical
            // Japanese rendering. Pure MOBI6 stays on `MobiImporter`.
            Format::Mobi => {
                let file = std::fs::File::open(path)?;
                let source: Arc<dyn crate::io::ByteSource> =
                    Arc::new(crate::io::FileSource::new(file)?);
                if crate::formats::mobi::sniff_format(&*source)?.is_kf8() {
                    Box::new(Azw3Importer::from_source(source)?)
                } else {
                    Box::new(MobiImporter::from_source(source)?)
                }
            }
            Format::Kfx => {
                // `.kfx-zip` bundles are merged into a single in-memory KFX
                // container before import. This unifies per-container symbol
                // tables — without that, references that span containers (the
                // newer-schema symbols beyond boko-kai's static table among
                // them) fail to resolve. See `kfx::merge` for the algorithm.
                if path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("kfx-zip"))
                {
                    let bytes = crate::formats::kfx::merge::merge_kfx_zip(path)?;
                    let source = Arc::new(MemorySource::new(bytes));
                    Box::new(KfxImporter::from_source(source)?)
                } else {
                    Box::new(KfxImporter::open(path)?)
                }
            }
            Format::Markdown => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Markdown format is export-only",
                ));
            }
        };
        Ok(Self {
            backend,
            ir_cache: Arc::new(RwLock::new(HashMap::new())),
            meta_override: None,
            image_color_mode: jxr::ColorMode::Color,
        })
    }

    /// Create a Book from in-memory bytes with an explicit format.
    ///
    /// This is useful for reading from stdin or other non-file sources.
    pub fn from_bytes(data: &[u8], format: Format) -> io::Result<Self> {
        let source: Arc<dyn crate::io::ByteSource> = Arc::new(MemorySource::new(data.to_vec()));
        let backend: Box<dyn Importer> = match format {
            Format::Epub => Box::new(EpubImporter::from_source(source)?),
            Format::Azw3 => Box::new(Azw3Importer::from_source(source)?),
            // See the matching arm in `open_format` for the routing rationale.
            // The sniff is cheap (PDB + record 0); branching here keeps the
            // dispatch on a single .mobi extension across both entry points.
            Format::Mobi => {
                if crate::formats::mobi::sniff_format(&*source)?.is_kf8() {
                    Box::new(Azw3Importer::from_source(source)?)
                } else {
                    Box::new(MobiImporter::from_source(source)?)
                }
            }
            Format::Kfx => Box::new(KfxImporter::from_source(source)?),
            Format::Markdown => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Markdown format is export-only",
                ));
            }
        };
        Ok(Self {
            backend,
            ir_cache: Arc::new(RwLock::new(HashMap::new())),
            meta_override: None,
            image_color_mode: jxr::ColorMode::Color,
        })
    }

    /// Book metadata. Returns the [override][Book::set_metadata_override] when
    /// one has been installed, otherwise the backend's parsed metadata.
    pub fn metadata(&self) -> &Metadata {
        self.meta_override
            .as_ref()
            .unwrap_or_else(|| self.backend.metadata())
    }

    /// Shadow the backend's parsed metadata with `meta`. Every later
    /// [`metadata`][Book::metadata] call — including the ones exporters make to
    /// build the output's title/author/identifier — sees `meta` instead. The
    /// source file is untouched: this only changes what *this* handle reports.
    ///
    /// Sidle uses it to bake edited library metadata into a (re)converted KFX
    /// without rewriting the source EPUB. Build the override by cloning
    /// `metadata()` and overwriting just the fields you mean to change, so
    /// untouched fields (identifier, ASIN, cover) survive.
    pub fn set_metadata_override(&mut self, meta: Metadata) {
        self.meta_override = Some(meta);
    }

    /// How raster images are encoded into a KFX export (default `Grayscale`).
    pub fn image_color_mode(&self) -> jxr::ColorMode {
        self.image_color_mode
    }

    /// Choose how raster images are encoded into a KFX export. `Grayscale`
    /// (default) emits `8bppGray` JXR — the device is B&W and the source keeps
    /// the color master; `Color` emits full `24bppRGB` JXR (channels that are
    /// identical everywhere still collapse to grayscale automatically). The
    /// cover stays JPEG regardless. Flipping this + reconverting is how a color
    /// book gets a color KFX for the Sidle reader.
    pub fn set_image_color_mode(&mut self, mode: jxr::ColorMode) {
        self.image_color_mode = mode;
    }

    /// Table of contents.
    pub fn toc(&self) -> &[TocEntry] {
        self.backend.toc()
    }

    /// Physical page-list (EPUB `<nav epub:type="page-list">`); empty if absent.
    pub fn page_list(&self) -> &[TocEntry] {
        self.backend.page_list()
    }

    /// Landmarks (structural navigation points).
    pub fn landmarks(&self) -> &[Landmark] {
        self.backend.landmarks()
    }

    /// Reading order (spine).
    pub fn spine(&self) -> &[SpineEntry] {
        self.backend.spine()
    }

    /// Get the internal source path for a chapter.
    pub fn source_id(&self, id: ChapterId) -> Option<&str> {
        self.backend.source_id(id)
    }

    /// The document `<title>` for a spine chapter (see
    /// [`crate::import::Importer::chapter_title`]).
    pub fn chapter_title(&self, id: ChapterId) -> Option<&str> {
        self.backend.chapter_title(id)
    }

    /// Load raw chapter bytes.
    pub fn load_raw(&mut self, id: ChapterId) -> io::Result<Vec<u8>> {
        self.backend.load_raw(id)
    }

    /// Load a chapter as normalized IR.
    ///
    /// This parses the chapter's HTML content and any linked or inline CSS,
    /// producing a normalized tree structure suitable for rendering.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use boko::{Book, Role};
    ///
    /// let mut book = Book::open("input.epub")?;
    /// let spine: Vec<_> = book.spine().to_vec();
    ///
    /// for entry in spine {
    ///     let chapter = book.load_chapter(entry.id)?;
    ///     for id in chapter.iter_dfs() {
    ///         let node = chapter.node(id).unwrap();
    ///         if matches!(node.role, Role::Heading(_)) {
    ///             // Process heading...
    ///         }
    ///     }
    /// }
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn load_chapter(&mut self, id: ChapterId) -> io::Result<Chapter> {
        self.backend.load_chapter(id)
    }

    /// Load a chapter as IR with caching.
    ///
    /// This method caches parsed IR chapters to avoid re-parsing when the same
    /// chapter is loaded multiple times (e.g., during normalized export).
    /// Returns an `Arc<Chapter>` for cheap cloning and thread-safe sharing.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use boko::Book;
    ///
    /// let mut book = Book::open("input.epub")?;
    /// let spine: Vec<_> = book.spine().to_vec();
    ///
    /// // First call parses the chapter
    /// let chapter1 = book.load_chapter_cached(spine[0].id)?;
    ///
    /// // Second call returns cached version (cheap Arc clone)
    /// let chapter2 = book.load_chapter_cached(spine[0].id)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn load_chapter_cached(&mut self, id: ChapterId) -> io::Result<Arc<Chapter>> {
        // Fast path: check read lock first
        {
            let cache = self
                .ir_cache
                .read()
                .map_err(|_| io::Error::other("IR cache lock poisoned"))?;
            if let Some(chapter) = cache.get(&id) {
                return Ok(Arc::clone(chapter));
            }
        }

        // Slow path: load chapter (no lock held during IO)
        let chapter = self.backend.load_chapter(id)?;
        let chapter_arc = Arc::new(chapter);

        // Write to cache
        {
            let mut cache = self
                .ir_cache
                .write()
                .map_err(|_| io::Error::other("IR cache lock poisoned"))?;
            cache.insert(id, Arc::clone(&chapter_arc));
        }

        Ok(chapter_arc)
    }

    /// Load several chapters as IR with caching, one bulk backend call for
    /// the misses — importers with thread-safe internals build those in
    /// parallel (`Importer::load_chapters`). Results come back in input
    /// order; the first backend error aborts (matching a serial `?` loop).
    pub fn load_chapters_cached(&mut self, ids: &[ChapterId]) -> io::Result<Vec<Arc<Chapter>>> {
        let misses: Vec<ChapterId> = {
            let cache = self
                .ir_cache
                .read()
                .map_err(|_| io::Error::other("IR cache lock poisoned"))?;
            let mut seen = std::collections::HashSet::new();
            ids.iter()
                .filter(|id| !cache.contains_key(id) && seen.insert(**id))
                .copied()
                .collect()
        };

        if !misses.is_empty() {
            let loaded = self.backend.load_chapters(&misses);
            let mut cache = self
                .ir_cache
                .write()
                .map_err(|_| io::Error::other("IR cache lock poisoned"))?;
            for (id, chapter) in misses.iter().zip(loaded) {
                cache.insert(*id, Arc::new(chapter?));
            }
        }

        let cache = self
            .ir_cache
            .read()
            .map_err(|_| io::Error::other("IR cache lock poisoned"))?;
        ids.iter()
            .map(|id| {
                cache
                    .get(id)
                    .map(Arc::clone)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Chapter not found"))
            })
            .collect()
    }

    /// Clear the IR cache.
    ///
    /// Call this to free memory after normalized export is complete.
    pub fn clear_cache(&mut self) {
        if let Ok(mut cache) = self.ir_cache.write() {
            cache.clear();
        }
    }

    /// Resolve all internal links in the book.
    ///
    /// Uses `load_chapter_cached()` internally, so chapters are parsed once
    /// and reused for subsequent export operations. Call this before export
    /// to benefit from caching.
    ///
    /// Returns both forward mappings (source -> target) and reverse mappings
    /// (target -> sources) for efficient lookup during traversal.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use boko::Book;
    ///
    /// let mut book = Book::open("input.epub")?;
    /// let resolved = book.resolve_links()?;
    ///
    /// // Check for broken links
    /// for (source, href) in resolved.broken_links() {
    ///     eprintln!("Broken link at {:?}: {}", source, href);
    /// }
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn resolve_links(&mut self) -> io::Result<ResolvedLinks> {
        resolve_book_links(self)
    }

    /// Index anchors for link resolution.
    ///
    /// Called internally by `resolve_links()`. Delegates to the format-specific
    /// importer to build anchor maps.
    pub(crate) fn index_anchors(&mut self, chapters: &[(ChapterId, Arc<Chapter>)]) {
        self.backend.index_anchors(chapters);
    }

    /// Resolve TOC hrefs (fills in fragments for AZW3/MOBI).
    ///
    /// Called internally by `resolve_links()`. Delegates to the format-specific
    /// importer.
    pub(crate) fn resolve_toc(&mut self) {
        self.backend.resolve_toc();
    }

    /// Resolve TOC entry targets using `resolve_href()`.
    ///
    /// Called internally by `resolve_links()`. Walks TOC entries and sets their
    /// `target` field.
    pub(crate) fn resolve_toc_targets(&mut self) {
        // First, collect all hrefs with their targets
        fn collect_targets(
            entries: &[TocEntry],
            backend: &dyn Importer,
            default_chapter: ChapterId,
            results: &mut Vec<Option<AnchorTarget>>,
        ) {
            for entry in entries {
                results.push(backend.resolve_toc_href(default_chapter, &entry.href));
                collect_targets(&entry.children, backend, default_chapter, results);
            }
        }

        let mut targets = Vec::new();
        collect_targets(
            self.backend.toc(),
            &*self.backend,
            ChapterId(0),
            &mut targets,
        );

        // Then apply the targets to the TOC entries
        fn apply_targets(
            entries: &mut [TocEntry],
            targets: &mut impl Iterator<Item = Option<AnchorTarget>>,
        ) {
            for entry in entries {
                entry.target = targets.next().flatten();
                apply_targets(&mut entry.children, targets);
            }
        }

        let toc = self.backend.toc_mut();
        apply_targets(toc, &mut targets.into_iter());
    }

    /// Resolve page-list entry targets using `resolve_href()`.
    ///
    /// The flat sibling of [`Self::resolve_toc_targets`]: walks the page-list
    /// entries and sets each `target` so the KFX exporter can look up its
    /// content position. Called internally by `resolve_links()`.
    pub(crate) fn resolve_page_list_targets(&mut self) {
        // Clone hrefs first so the resolve pass holds no borrow of the page
        // list while we later take it mutably to write the targets back.
        let hrefs: Vec<String> = self
            .backend
            .page_list()
            .iter()
            .map(|e| e.href.clone())
            .collect();
        let targets: Vec<Option<AnchorTarget>> = hrefs
            .iter()
            .map(|href| self.backend.resolve_toc_href(ChapterId(0), href))
            .collect();
        for (entry, target) in self.backend.page_list_mut().iter_mut().zip(targets) {
            entry.target = target;
        }
    }

    /// Resolve a single href using format-specific logic.
    ///
    /// Called internally by `resolve_links()`. Delegates to the format-specific
    /// importer.
    pub(crate) fn resolve_href(&self, from_chapter: ChapterId, href: &str) -> Option<AnchorTarget> {
        self.backend.resolve_href(from_chapter, href)
    }

    /// Resolve a navigation href (TOC / page-list / landmarks), falling back to
    /// the chapter start when a `path#fragment`'s file resolves but its fragment
    /// is dead. See [`crate::import::Importer::resolve_toc_href`].
    pub(crate) fn resolve_toc_href(
        &self,
        from_chapter: ChapterId,
        href: &str,
    ) -> Option<AnchorTarget> {
        self.backend.resolve_toc_href(from_chapter, href)
    }

    /// The fragment id a navigation href carries in normalized export, plus
    /// whether it was stamped into content. See
    /// [`crate::import::Importer::nav_fragment`].
    pub(crate) fn nav_fragment(&self, href: &str) -> Option<(String, bool)> {
        self.backend.nav_fragment(href)
    }

    /// The source's named-style program for normalized export. See
    /// [`crate::import::Importer::stylesheet_program`].
    pub(crate) fn stylesheet_program(&mut self) -> Option<crate::import::CssProgram> {
        self.backend.stylesheet_program()
    }

    /// Load an asset by path.
    pub fn load_asset(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        self.backend.load_asset(path)
    }

    /// Load several assets, one result per input path (implementations may
    /// parallelize expensive per-asset work, e.g. KFX image transcodes).
    pub fn load_assets(&mut self, paths: &[std::path::PathBuf]) -> Vec<io::Result<Vec<u8>>> {
        self.backend.load_assets(paths)
    }

    /// List all assets.
    pub fn list_assets(&self) -> &[std::path::PathBuf] {
        self.backend.list_assets()
    }

    /// The authoritative asset list for a normalized EPUB export (`None`
    /// when the importer defers to the assets the content references).
    pub fn bundled_assets(&self) -> Option<Vec<std::path::PathBuf>> {
        self.backend.bundled_assets()
    }

    /// Collect all @font-face definitions from CSS files.
    ///
    /// Returns font-face rules that map font family names to font files.
    /// Used by KFX export to create font entities linking font-family
    /// names to resource locations.
    pub fn font_faces(&mut self) -> Vec<crate::model::FontFace> {
        self.backend.font_faces()
    }

    /// Whether this book requires normalized export for HTML-based formats.
    ///
    /// Returns true for binary formats (KFX) where the raw content is not HTML.
    /// Exporters should use IR-based output when this returns true.
    pub fn requires_normalized_export(&self) -> bool {
        self.backend.requires_normalized_export()
    }

    /// Export the book to a different format.
    ///
    /// # Supported Export Formats
    ///
    /// | Format   | Support |
    /// |----------|---------|
    /// | EPUB     | ✓       |
    /// | KFX      | ✓       |
    /// | Markdown | ✓       |
    /// | AZW3     | ✗       |
    /// | MOBI     | ✗       |
    ///
    /// # Example
    ///
    /// ```no_run
    /// use boko::{Book, Format};
    /// use std::fs::File;
    ///
    /// let mut book = Book::open("input.azw3")?;
    /// let mut file = File::create("output.epub")?;
    /// book.export(Format::Epub, &mut file)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn export<W: Write + Seek>(&mut self, format: Format, writer: &mut W) -> io::Result<()> {
        self.export_with_progress(format, writer, &|_, _, _, _| {})
    }

    /// Like [`Book::export`], but reports coarse conversion progress to
    /// `on_progress` as `(phase_key, current, total, human_label)` — sidle's
    /// conversion queue uses this to drive a determinate progress bar. Only KFX
    /// export (the slow direction) emits phases today; EPUB export ignores the
    /// callback.
    pub fn export_with_progress<W: Write + Seek>(
        &mut self,
        format: Format,
        writer: &mut W,
        on_progress: &dyn Fn(&str, usize, usize, &str),
    ) -> io::Result<()> {
        match format {
            Format::Epub => EpubExporter::new().export(self, writer),
            Format::Kfx => KfxExporter::new().export_with_progress(self, writer, on_progress),
            Format::Markdown => MarkdownExporter::new().export(self, writer),
            Format::Azw3 | Format::Mobi => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{:?} export is not supported", format),
            )),
        }
    }
}

// ============================================================================
// Constructors
// ============================================================================

impl TocEntry {
    pub fn new(title: impl Into<String>, href: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            href: href.into(),
            children: Vec::new(),
            play_order: None,
            target: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_override_shadows_backend_keeps_other_fields() {
        let bytes = std::fs::read("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let mut book = Book::from_bytes(&bytes, Format::Epub).unwrap();

        let backend_lang = book.metadata().language.clone();
        assert_ne!(book.metadata().title, "SHADOWED", "fixture sanity");

        let mut over = book.metadata().clone();
        over.title = "SHADOWED".into();
        book.set_metadata_override(over);

        // The override is what every later read (and thus every exporter) sees…
        assert_eq!(book.metadata().title, "SHADOWED");
        // …while fields we copied from the backend are unchanged.
        assert_eq!(book.metadata().language, backend_lang);
    }
}
