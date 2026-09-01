//! Core data types and runtime handle for ebooks: [`Book`] over a format's
//! [`crate::import::Importer`], with the [`Metadata`], [`TocEntry`],
//! [`Landmark`] and [`SpineEntry`] it reaches through one surface.

use std::collections::HashMap;
use std::io::{self, Seek, Write};
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::export::{Azw3Exporter, EpubExporter, Exporter, KfxExporter, MarkdownExporter};
use crate::import::{
    Azw3Importer, ChapterId, EpubImporter, Importer, KfxImporter, MobiImporter, SpineEntry,
};
use crate::io::MemorySource;
use crate::model::resolved::resolve_book_links;
use crate::model::{AnchorTarget, Chapter, ResolvedLinks};

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

/// Which flavour of periodical a book is, as Amazon's catalogue names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeriodicalKind {
    /// MOBI header type 259, `cdetype = MAGZ`.
    Magazine,
    /// MOBI header type 257, `cdetype = NWPR`.
    Newspaper,
    /// MOBI header type 258, `cdetype = FEED`.
    Blog,
}

impl PeriodicalKind {
    /// The four-letter content type KFX writes as `cde_content_type`.
    pub fn cde_type(self) -> &'static str {
        match self {
            PeriodicalKind::Magazine => "MAGZ",
            PeriodicalKind::Newspaper => "NWPR",
            PeriodicalKind::Blog => "FEED",
        }
    }

    /// From an EXTH 501 / KFX `cde_content_type` value.
    pub fn from_cde_type(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "MAGZ" => Some(PeriodicalKind::Magazine),
            "NWPR" => Some(PeriodicalKind::Newspaper),
            "FEED" => Some(PeriodicalKind::Blog),
            _ => None,
        }
    }

    /// From a MOBI header type: 257 `News`, 258 `News_Feed`, 259
    /// `News_Magazine`. Everything else is a book.
    pub fn from_mobi_type(t: u32) -> Option<Self> {
        match t {
            257 => Some(PeriodicalKind::Newspaper),
            258 => Some(PeriodicalKind::Blog),
            259 => Some(PeriodicalKind::Magazine),
            _ => None,
        }
    }
}

/// Book metadata (Dublin Core + extensions)
#[derive(Debug, Clone, Default)]
pub struct Metadata {
    pub title: String,
    pub authors: Vec<String>,
    pub language: String,
    pub identifier: String,
    /// Amazon Standard Identification Number, separate from `identifier`.
    /// An EPUB import reads it from `<dc:identifier opf:scheme="ASIN">`.
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
    /// Sort/pronunciation keys, positionally aligned with `authors`: EPUB
    /// `file-as`, KFX `author_pronunciation`, MOBI EXTH 517. Empty when the
    /// source declared none.
    pub author_sorts: Vec<String>,
    /// belongs-to-collection (series info)
    pub collection: Option<CollectionInfo>,
    /// OPF spine `page-progression-direction` attribute ("ltr" | "rtl" | "default").
    /// Required for vertical-RTL Japanese books to bind/swipe correctly on Kindle.
    pub page_progression_direction: Option<String>,
    /// OPF `<meta name="primary-writing-mode">` value: "vertical-rl" for
    /// Japanese, "vertical-lr" for Mongolian, both `rtl` in
    /// `page_progression_direction`.
    pub primary_writing_mode: Option<String>,
    /// `rendition:layout = pre-paginated`. Selects the FXL OPF metadata on
    /// EPUB export and the `yj_non_pdf_fixed_layout` skeleton on KFX export.
    pub fixed_layout: bool,
    /// `book-type` OPF hint — `"comic"` for double-page-spread manga,
    /// `"children"` for picture books. Only meaningful when `fixed_layout`.
    pub book_type: Option<String>,
    /// `rendition:spread` policy for a fixed-layout book, e.g. `"landscape"`
    /// for facing pages as a two-page spread.
    pub rendition_spread: Option<String>,
    /// The `OrientationLock` the source declared. Read with `fixed_layout`.
    pub orientation_lock: Option<OrientationLock>,
    /// Book-level default page viewport `(width, height)` in px — the
    /// `fixed-layout-jp:viewport` / KF8 `original-resolution`. Every FXL page
    /// is authored to this box unless it carries its own viewport.
    pub default_viewport: Option<(u32, u32)>,
    /// `PeriodicalKind` for an issue of a periodical, `None` for a book. Its
    /// issue date is `date`; `title` names the magazine, and a sideload with
    /// no cdeGroup stacks on that shared title.
    pub periodical: Option<PeriodicalKind>,
}

/// A declared screen-orientation lock. `kindle_value` spells it for KF8
/// `orientation-lock` and KFX `book_orientation_lock`, `rendition_value` for
/// EPUB 3 `rendition:orientation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrientationLock {
    /// `none` in the Kindle vocabulary, `auto` in EPUB.
    Auto,
    Portrait,
    Landscape,
}

impl OrientationLock {
    /// `none` / `portrait` / `landscape`.
    pub fn kindle_value(self) -> &'static str {
        match self {
            OrientationLock::Auto => "none",
            OrientationLock::Portrait => "portrait",
            OrientationLock::Landscape => "landscape",
        }
    }

    /// `auto` / `portrait` / `landscape`.
    pub fn rendition_value(self) -> &'static str {
        match self {
            OrientationLock::Auto => "auto",
            OrientationLock::Portrait => "portrait",
            OrientationLock::Landscape => "landscape",
        }
    }

    /// Parse `none` / `auto` / `portrait` / `landscape`, case-insensitive.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "auto" => Some(OrientationLock::Auto),
            "portrait" => Some(OrientationLock::Portrait),
            "landscape" => Some(OrientationLock::Landscape),
            _ => None,
        }
    }
}

/// A rectangle on a fixed-layout page, as fractions of the page box: `0.0` is
/// its left/top edge and `1.0` its right/bottom. The fields are signed: a
/// magnified view's image runs negative and past `1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelRect {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

/// One author-drawn comic panel on a fixed-layout page: a KF8
/// `app-amzn-magnify` region, a KFX `zoom_panel`. Tapping `source` magnifies
/// the page to fill `window` with `image`; `ordinal` orders the panels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Panel {
    /// Reading order within the page, as the source numbers it.
    pub ordinal: u32,
    /// The panel's own rectangle on the page.
    pub source: PanelRect,
    /// The rectangle the magnified view occupies, in page fractions.
    pub window: PanelRect,
    /// The page image at magnified scale, in fractions of `window`.
    pub image: PanelRect,
}

/// Which half of a two-page spread a fixed-layout page occupies — the OPF
/// spine itemref `page-spread-left` / `page-spread-right` /
/// `rendition:page-spread-center` property.
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

impl crate::model::toc_shape::TocNode for TocEntry {
    fn label(&self) -> &str {
        &self.title
    }
    fn set_label(&mut self, label: String) {
        self.title = label;
    }
    fn children(&self) -> &[Self] {
        &self.children
    }
    fn set_children(&mut self, children: Vec<Self>) {
        self.children = children;
    }
    /// Two entries land on the same place when they name the same href — the
    /// fragment included, since two anchors in one document are two chapters.
    fn target_key(&self) -> String {
        self.href.clone()
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

impl LandmarkType {
    /// Human-readable name for a landmark whose source carried no label of its
    /// own: a cover marked with a placeholder, or a reading-start marker
    /// carrying nothing at all.
    pub fn default_label(self) -> &'static str {
        match self {
            Self::Cover => "Cover",
            Self::TitlePage => "Title Page",
            Self::Toc => "Table of Contents",
            Self::FrontMatter => "Front Matter",
            Self::BackMatter => "Back Matter",
            Self::Acknowledgements => "Acknowledgments",
            Self::Bibliography => "Bibliography",
            Self::Glossary => "Glossary",
            Self::Index => "Index",
            Self::Preface => "Preface",
            Self::Endnotes => "Notes",
            Self::Loi => "List of Illustrations",
            Self::Lot => "List of Tables",
            Self::StartReading | Self::BodyMatter => "Start of Content",
        }
    }

    /// Whether this landmark names a part of the book, belonging in a chapter
    /// list. `StartReading` and `BodyMatter` mark a place the chapter list
    /// carries under the chapter's own name.
    pub fn names_a_section(self) -> bool {
        !matches!(self, Self::StartReading | Self::BodyMatter)
    }
}

/// A landmark navigation entry: a structural location in a book — cover,
/// start of content, endnotes — that navigation and reader features target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landmark {
    /// Type of landmark
    pub landmark_type: LandmarkType,
    /// Target href (file path with optional fragment)
    pub href: String,
    /// Display label
    pub label: String,
    /// Resolved target (set by `resolve_links()`)
    pub target: Option<AnchorTarget>,
}

// ============================================================================

/// Runtime handle for an ebook: `Book` wraps a format-specific `Importer` and
/// reaches its metadata, table of contents and content through one surface.
pub struct Book {
    backend: Box<dyn Importer>,
    /// Cache of parsed IR chapters to avoid re-parsing during normalized export.
    /// Uses RwLock for thread-safe access and Arc for cheap cloning.
    ir_cache: Arc<RwLock<HashMap<ChapterId, Arc<Chapter>>>>,
    /// Caller-supplied metadata that shadows the backend's parsed metadata.
    /// When set, [`Book::metadata`] returns it. `None` = the backend's
    /// metadata verbatim.
    meta_override: Option<Metadata>,
    /// How raster images are encoded into a KFX export: `Color` (default,
    /// `24bppRGB` JXR, collapsed to `8bppGray` when the channels are
    /// identical) or `Grayscale` (`8bppGray`).
    image_color_mode: jxr::ColorMode,
    /// Worker-thread cap for every parallel stage of an import or export off
    /// this handle. `0` = the platform's reported parallelism. Set via
    /// [`Book::set_max_workers`].
    max_workers: usize,
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
                // `.pobi` is a plain MOBI6 container, marked as a periodical
                // by its MOBI header type, EXTH 501 `cdetype` and a
                // hierarchical NCX.
                "mobi" | "pobi" => Some(Format::Mobi),
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
        matches!(
            self,
            Format::Epub | Format::Kfx | Format::Markdown | Format::Azw3
        )
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
            // `Format::Mobi` covers pure MOBI6, pure KF8, and the two
            // combined. `Azw3Importer` carries the KF8 shapes' CSS, spine and
            // per-chapter `xml:lang`; `MobiImporter` takes pure MOBI6.
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
                // `kfx::merge` folds a `.kfx-zip` bundle into one in-memory
                // container, unifying the per-container symbol tables a
                // cross-container reference resolves through.
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
            max_workers: 0,
        })
    }

    /// Create a Book from borrowed in-memory bytes with an explicit format,
    /// copying them into the handle. [`Book::from_vec`] takes ownership and
    /// copies nothing.
    pub fn from_bytes(data: &[u8], format: Format) -> io::Result<Self> {
        Self::from_vec(data.to_vec(), format)
    }

    /// Create a Book from owned in-memory bytes with an explicit format,
    /// taking the buffer as the handle's byte source. The entry point for
    /// stdin and other non-file sources.
    pub fn from_vec(data: Vec<u8>, format: Format) -> io::Result<Self> {
        let source: Arc<dyn crate::io::ByteSource> = Arc::new(MemorySource::new(data));
        let backend: Box<dyn Importer> = match format {
            Format::Epub => Box::new(EpubImporter::from_source(source)?),
            Format::Azw3 => Box::new(Azw3Importer::from_source(source)?),
            // `Format::Mobi` covers pure MOBI6, pure KF8 and the two combined:
            // `sniff_format` reads the PDB header and record 0 to tell them
            // apart, as the matching arm in `open_format` does.
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
            max_workers: 0,
        })
    }

    /// A Book over an [`Importer`] the caller built. [`Book::open_format`]
    /// and [`Book::from_vec`] pick a backend for a [`Format`]; this takes
    /// any [`Importer`].
    pub fn from_importer(backend: Box<dyn Importer>) -> Self {
        Self {
            backend,
            ir_cache: Arc::new(RwLock::new(HashMap::new())),
            meta_override: None,
            image_color_mode: jxr::ColorMode::Color,
            max_workers: 0,
        }
    }

    /// Book metadata. Returns the [override][Book::set_metadata_override] when
    /// one has been installed, else the backend's parsed metadata.
    pub fn metadata(&self) -> &Metadata {
        self.meta_override
            .as_ref()
            .unwrap_or_else(|| self.backend.metadata())
    }

    /// Shadow the backend's parsed metadata with `meta`; every later
    /// [`metadata`][Book::metadata] call sees it, and the source file is
    /// untouched. Clone `metadata()` to keep identifier, ASIN and cover.
    pub fn set_metadata_override(&mut self, meta: Metadata) {
        self.meta_override = Some(meta);
    }

    /// How raster images are encoded into a KFX export (default `Color`).
    pub fn image_color_mode(&self) -> jxr::ColorMode {
        self.image_color_mode
    }

    /// Cap the worker threads one parallel import or export stage may run at
    /// once; `0` restores the platform's reported parallelism. Each worker
    /// holds one job's working set, and the cap bounds peak memory.
    pub fn set_max_workers(&mut self, workers: usize) {
        self.max_workers = workers;
        self.backend.set_max_workers(workers);
    }

    /// The worker cap in force, `0` for the platform's reported parallelism.
    /// See [`Book::set_max_workers`].
    pub fn max_workers(&self) -> usize {
        self.max_workers
    }

    /// Choose how raster images encode into a KFX export. `Grayscale` emits
    /// `8bppGray` JXR, `Color` emits `24bppRGB` JXR, and an image whose
    /// channels match everywhere collapses to grayscale. The cover stays JPEG.
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

    /// Load a chapter as normalized IR: its HTML content and any linked or
    ///
    /// inline CSS, parsed into a tree.
    pub fn load_chapter(&mut self, id: ChapterId) -> io::Result<Chapter> {
        self.backend.load_chapter(id)
    }

    /// Load a chapter as IR, holding each parsed chapter in an [`Arc`] cache
    /// keyed by [`ChapterId`]. A second call for the same id clones the handle.
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

    /// Load several chapters as IR with caching, one bulk
    /// `Importer::load_chapters` call for the misses. Results come back in
    /// input order; the first backend error aborts.
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
    pub fn resolve_links(&mut self) -> io::Result<ResolvedLinks> {
        resolve_book_links(self)
    }

    /// Index anchors for link resolution, through the format-specific
    /// importer. `resolve_links` calls this.
    pub(crate) fn index_anchors(&mut self, chapters: &[(ChapterId, Arc<Chapter>)]) {
        self.backend.index_anchors(chapters);
    }

    /// Resolve TOC hrefs through the format-specific importer, filling in an
    /// AZW3/MOBI fragment. `resolve_links` calls this.
    pub(crate) fn resolve_toc(&mut self) {
        self.backend.resolve_toc();
    }

    /// Walk the TOC entries, setting each `target` from `resolve_href`.
    /// `resolve_links` calls this.
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

    /// The flat sibling of [`Self::resolve_toc_targets`]: walks the page-list
    /// entries, setting each `target` from `resolve_href` to the content
    /// position the KFX exporter looks up.
    pub(crate) fn resolve_page_list_targets(&mut self) {
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

    /// The flat sibling of [`Self::resolve_toc_targets`] for the landmarks:
    /// walks them, setting each `target` from `resolve_toc_href`.
    pub(crate) fn resolve_landmark_targets(&mut self) {
        let hrefs: Vec<String> = self
            .backend
            .landmarks()
            .iter()
            .map(|landmark| landmark.href.clone())
            .collect();
        let targets: Vec<Option<AnchorTarget>> = hrefs
            .iter()
            .map(|href| self.backend.resolve_toc_href(ChapterId(0), href))
            .collect();
        for (landmark, target) in self.backend.landmarks_mut().iter_mut().zip(targets) {
            landmark.target = target;
        }
    }

    /// Resolve a single href through the format-specific importer.
    /// `resolve_links` calls this.
    pub(crate) fn resolve_href(&self, from_chapter: ChapterId, href: &str) -> Option<AnchorTarget> {
        self.backend.resolve_href(from_chapter, href)
    }

    /// Resolve a navigation href (TOC / page-list / landmarks), falling back to
    /// the chapter start when a `path#fragment`'s file resolves but its fragment
    /// is dead. See [`crate::import::Importer::resolve_toc_href`].
    pub fn resolve_toc_href(&self, from_chapter: ChapterId, href: &str) -> Option<AnchorTarget> {
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

    /// The axis the book states it is written along. See
    /// [`crate::import::Importer::writing_mode`].
    pub fn writing_mode(&mut self) -> crate::style::WritingMode {
        self.backend.writing_mode()
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

    /// The same assets as the source stores them, each with its declared
    /// format. See [`crate::import::Importer::load_assets_stored`].
    pub fn load_assets_stored(
        &mut self,
        paths: &[std::path::PathBuf],
    ) -> Vec<io::Result<(Vec<u8>, Option<String>)>> {
        self.backend.load_assets_stored(paths)
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

    /// [`Self::bundled_assets`] with each entry's predicted media type and
    /// declared pixel size, without loading any of them — see
    /// [`AssetInfo`](crate::import::AssetInfo).
    pub fn asset_manifest(&mut self) -> Option<Vec<crate::import::AssetInfo>> {
        self.backend.asset_manifest()
    }

    /// The `@font-face` rules the CSS files declare, mapping a font family
    /// name to its file. A KFX export builds one `font` entity per rule.
    pub fn font_faces(&mut self) -> Vec<crate::model::FontFace> {
        self.backend.font_faces()
    }

    /// Whether an HTML-based export takes the normalized route: true for a
    /// binary source such as KFX, whose raw content is not HTML.
    pub fn requires_normalized_export(&self) -> bool {
        self.backend.requires_normalized_export()
    }

    /// The book's reading-position scale, or `None` when the source defines
    /// none. See [`crate::model::PositionMap`] and
    /// [`crate::import::Importer::position_map`].
    pub fn position_map(&mut self) -> Option<crate::model::PositionMap> {
        self.backend.position_map()
    }

    /// The source's own base text, keyed by the element ids
    /// [`Self::position_map`] places. See [`crate::model::SourceText`] and
    /// [`crate::import::Importer::source_text`].
    pub fn source_text(&mut self) -> Option<crate::model::SourceText> {
        self.backend.source_text()
    }

    /// Export the book to a different format.
    pub fn export<W: Write + Seek>(&mut self, format: Format, writer: &mut W) -> io::Result<()> {
        self.export_with_progress(format, writer, &|_, _, _, _| {})
    }

    /// [`Book::export`], reporting `(phase_key, current, total, human_label)`
    /// to `on_progress`. A KFX export emits `survey → chapters → images →
    /// finalize`, an EPUB one `content → resources → … → finalize`.
    pub fn export_with_progress<W: Write + Seek>(
        &mut self,
        format: Format,
        writer: &mut W,
        on_progress: &dyn Fn(&str, usize, usize, &str),
    ) -> io::Result<()> {
        match format {
            Format::Epub => EpubExporter::new().export_with_progress(self, writer, on_progress),
            Format::Kfx => KfxExporter::new().export_with_progress(self, writer, on_progress),
            Format::Markdown => MarkdownExporter::new().export(self, writer),
            Format::Azw3 => Azw3Exporter::new().export(self, writer),
            Format::Mobi => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{:?} export is not supported", format),
            )),
        }
    }
}

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

        // `metadata` reads the override's title and the backend's language.
        assert_eq!(book.metadata().title, "SHADOWED");
        assert_eq!(book.metadata().language, backend_lang);
    }
}
