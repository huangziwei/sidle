//! EPUB emission: the exporter plus everything it writes with.
//!
//! This module (`mod.rs`) is the `EpubExporter` itself — raw passthrough
//! and normalized routes, zip assembly, package wiring. Its children hold
//! the pieces: shared document emitters ([`opf`], [`nav`], `titlepage`),
//! the normalization pipeline (`normalize`), and the two synthesis
//! regimes (`synth` string-based with pool-derived `.c<N>` classes;
//! [`dom`] + [`dom_synth`] DOM-based for source-declared style programs).

pub mod dom;
pub mod dom_synth;
pub mod nav;
pub(crate) mod normalize;
pub mod opf;
pub(crate) mod synth;
pub(crate) mod titlepage;

pub use normalize::{
    ChapterContent, GlobalStylePool, NormalizedContent, SourceElements, normalize_book,
    normalize_book_with,
};

use std::collections::{HashMap, HashSet};
use std::io::{self, Seek, Write};
use std::path::Path;

use zip::CompressionMethod;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::formats::epub::page_shape;
use crate::model::{AnchorTarget, Book, LandmarkType, TocEntry};

use self::nav::NavPoint;
use self::opf::{
    OpfCollection, OpfCreator, OpfFixedLayout, OpfGuideRef, OpfItem, OpfItemref, OpfMetadata,
    OpfPackage,
};
use super::Exporter;

/// Configuration for EPUB export.
#[derive(Debug, Clone, Default)]
pub struct EpubConfig {
    /// Compression level for deflate (0-9, default 6).
    pub compression_level: Option<u32>,
    /// If true, normalize content through IR pipeline for clean, consistent output.
    /// Default is false (passthrough mode preserves original HTML/CSS).
    pub normalize: bool,
}

/// Whether a build produces asset bytes or only describes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assets {
    /// Load and transcode every asset. A container has to hold the bytes.
    Load,
    /// Describe them only — path, media type, declared pixel size. A renderer
    /// that streams a book fetches each asset when it needs it, which is the
    /// difference between opening a large illustrated book instantly and
    /// waiting out a full-book image decode.
    Describe,
}

/// How to build a package. Prefer the two named intents,
/// [`Self::container`] and [`Self::rendered`], over assembling one by hand:
/// the combinations that matter are the two that have a consumer.
#[derive(Debug, Clone, Copy)]
pub struct PackageOptions {
    /// Whether documents carry their source element ids.
    pub source_elements: SourceElements,
    /// Whether asset bytes are produced.
    pub assets: Assets,
}

impl PackageOptions {
    /// A package that can be written to an EPUB container: asset bytes
    /// produced, no source element ids in the documents.
    pub fn container() -> Self {
        Self {
            source_elements: SourceElements::Omit,
            assets: Assets::Load,
        }
    }

    /// A package for rendering rather than shipping: documents carry their
    /// source element ids so a stored `(element, offset)` handle can be
    /// resolved, and assets are described rather than loaded.
    /// [`EpubExporter::write_package`] rejects it.
    pub fn rendered() -> Self {
        Self {
            source_elements: SourceElements::Mark,
            assets: Assets::Describe,
        }
    }
}

/// One asset of a built package.
#[derive(Debug, Clone)]
pub struct PackageAsset {
    /// Path within `OEBPS/`.
    pub href: String,
    /// Media type. Sniffed from the bytes when they were loaded (a JPEG-XR
    /// that failed to decode passes through as `image/jxr` despite its name);
    /// the source's declared type otherwise.
    pub media_type: String,
    /// Pixel size: read out of the bytes on a build that loaded them, taken
    /// from the source's declaration on one that did not.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// `None` on a build that only described the assets — see [`Assets`] — and
    /// on one whose bytes left through an [`AssetSink`].
    pub bytes: Option<Vec<u8>>,
}

/// Where a build sends each asset's bytes.
///
/// [`build_package_into`] hands over one asset at a time, in manifest
/// registration order, keeping only the description on the package it
/// returns. A sink writing into the container leaves one chunk resident.
pub trait AssetSink {
    /// Take one asset's post-transcode bytes. `asset` describes them and is
    /// the entry that reaches the package and the manifest.
    fn take(&mut self, asset: &PackageAsset, bytes: Vec<u8>) -> io::Result<()>;
}

/// The [`AssetSink`] behind [`build_package`]: every asset's bytes, in the
/// order they were produced, for [`build_package`] to put back on the package.
#[derive(Default)]
struct CollectAssets(Vec<Vec<u8>>);

impl AssetSink for CollectAssets {
    fn take(&mut self, _asset: &PackageAsset, bytes: Vec<u8>) -> io::Result<()> {
        self.0.push(bytes);
        Ok(())
    }
}

/// One spine document of a built package.
#[derive(Debug, Clone)]
pub struct PackageDocument {
    /// Filename within `OEBPS/`.
    pub href: String,
    /// The complete XHTML document.
    pub xhtml: String,
    /// Spine `properties` — the fixed-layout page-spread pairing, when the
    /// importer declared one.
    pub spread: Option<String>,
    /// Fixed-layout page pixel box, when the source declared one.
    pub viewport: Option<(u32, u32)>,
}

/// A normalized book built into EPUB shape but not yet written to a container:
/// every document, stylesheet, and asset byte the zip would hold, in the order
/// it would hold them.
///
/// [`build_package`] produces it and [`EpubExporter`] zips it. Consumers that
/// render a book instead of shipping one take the same documents without the
/// container — which is what keeps a rendered book and an exported one from
/// drifting apart.
#[derive(Debug)]
pub struct EpubPackage {
    /// Every spine document the source produced, in reading order.
    pub documents: Vec<PackageDocument>,
    /// The synthesized SVG cover page, which occupies spine position 0 when
    /// present. Written last in the container (manifest registration order).
    pub titlepage: Option<String>,
    /// Index into [`Self::documents`] of the source's own cover page, when a
    /// [`Self::titlepage`] renders the same image.
    ///
    /// A publisher EPUB ships ONE cover, so a container must drop one of the
    /// two — and the package reports the overlap rather than resolving it,
    /// because the right one to keep depends on who is rendering:
    ///
    /// - A container drops this one. The source's page is a bare `<img>` that
    ///   inherits body margins in a foreign reader; the titlepage's SVG
    ///   `viewBox` is what makes a cover fill the page.
    /// - A renderer that resolves `(element, offset)` handles keeps this one
    ///   and ignores the titlepage, which carries no source elements and so
    ///   has no reading position at all.
    pub redundant_cover: Option<usize>,
    /// `content.opf`.
    pub opf: String,
    /// `nav.xhtml`.
    pub nav: String,
    /// `toc.ncx`.
    pub ncx: String,
    /// The unified stylesheet; empty when the book contributed no styles.
    pub css: String,
    /// Assets in manifest registration order, post-transcode.
    pub assets: Vec<PackageAsset>,
    /// The resolved TOC tree, hrefs pointing into [`Self::documents`], in the
    /// order the source declares its navigation — which is not necessarily
    /// spine order, and is not the order [`Self::nav`] emits.
    pub toc: Vec<NavPoint>,
    /// The document's CSS writing mode (`horizontal-tb`, `vertical-rl`,
    /// `vertical-lr`) — the value [`Self::css`] writes into its body rule.
    ///
    /// Not to be confused with the OPF `primary-writing-mode` metadatum in
    /// [`Self::opf`], which uses a different vocabulary that folds in page
    /// progression: an RTL manga is `horizontal-rl` there and `horizontal-tb`
    /// here. A renderer wants this one.
    pub writing_mode: String,
}

/// EPUB format exporter.
///
/// Creates standard EPUB files compatible with most e-readers.
///
/// # Example
///
/// ```no_run
/// use bokai::Book;
/// use bokai::export::{EpubExporter, Exporter};
/// use std::fs::File;
///
/// let mut book = Book::open("input.azw3")?;
/// let mut file = File::create("output.epub")?;
/// EpubExporter::new().export(&mut book, &mut file)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct EpubExporter {
    config: EpubConfig,
}

impl EpubExporter {
    /// Create a new exporter with default configuration.
    pub fn new() -> Self {
        Self {
            config: EpubConfig::default(),
        }
    }

    /// Configure the exporter with custom settings.
    pub fn with_config(mut self, config: EpubConfig) -> Self {
        self.config = config;
        self
    }
}

impl Default for EpubExporter {
    fn default() -> Self {
        Self::new()
    }
}

impl Exporter for EpubExporter {
    fn export<W: Write + Seek>(&self, book: &mut Book, writer: &mut W) -> io::Result<()> {
        self.export_with_progress(book, writer, &|_, _, _, _| {})
    }
}

impl EpubExporter {
    /// Like [`Exporter::export`], but reports coarse phase progress to
    /// `on_progress` as `(phase_key, current, total, human_label)` — see
    /// [`crate::Book::export_with_progress`]. Both routes emit the same phase
    /// sequence, `content → resources → nav → finalize`, so a caller's bar needs
    /// one band table either way; what each phase spends its time on differs
    /// (the normalized route transcodes images under `resources`, the raw one
    /// deflates the source's own files there).
    pub fn export_with_progress<W: Write + Seek>(
        &self,
        book: &mut Book,
        writer: &mut W,
        on_progress: &dyn Fn(&str, usize, usize, &str),
    ) -> io::Result<()> {
        // Use normalized mode if explicitly requested OR if the source format requires it
        // (e.g., KFX raw content is binary Ion, not HTML)
        if self.config.normalize || book.requires_normalized_export() {
            self.export_normalized(book, writer, on_progress)
        } else {
            self.export_raw(book, writer, on_progress)
        }
    }

    /// Export with passthrough mode (preserves original HTML/CSS).
    fn export_raw<W: Write + Seek>(
        &self,
        book: &mut Book,
        writer: &mut W,
        on_progress: &dyn Fn(&str, usize, usize, &str),
    ) -> io::Result<()> {
        // Resolve TOC fragments first (AZW3/MOBI backends refine NCX byte
        // positions into `#id` anchors; a no-op elsewhere). The
        // materialization path gets this via `resolve_links()`, but the raw
        // route never materializes — without it every nested nav/NCX entry
        // lands at its chapter start instead of the in-chapter target.
        book.resolve_toc();

        let mut zip = ZipWriter::new(writer);

        let compression_level = self.config.compression_level.unwrap_or(6);
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let deflated = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(compression_level as i64));

        // 1. Write mimetype (must be first, uncompressed)
        zip.start_file("mimetype", stored).map_err(io_error)?;
        zip.write_all(b"application/epub+zip")?;

        // 2. Write container.xml
        zip.start_file("META-INF/container.xml", deflated)
            .map_err(io_error)?;
        zip.write_all(CONTAINER_XML)?;

        // 3. Collect content info for manifest (hrefs relative to the OPF;
        // the emitter adds the fixed NCX/nav items itself)
        let spine: Vec<_> = book.spine().to_vec();
        let mut manifest_items: Vec<OpfItem> = Vec::new();
        let mut spine_items: Vec<OpfItemref> = Vec::new();

        // Add chapters to manifest. Chapter bytes load once here — the
        // OPF-014 property scan (a content doc embedding inline SVG / MathML
        // / scripting must declare it on its manifest item) needs the text,
        // and step 7 writes the same bytes.
        let mut chapter_bytes: Vec<Vec<u8>> = Vec::with_capacity(spine.len());
        for (i, entry) in spine.iter().enumerate() {
            on_progress("content", i + 1, spine.len(), "Reading chapters");
            let source_path = book
                .source_id(entry.id)
                .unwrap_or("unknown.xhtml")
                .to_string();
            let content = book.load_raw(entry.id)?;
            let id = format!("chapter_{}", i);
            manifest_items.push(OpfItem {
                id: id.clone(),
                href: sanitize_path(&source_path),
                media_type: "application/xhtml+xml".to_string(),
                properties: opf::xhtml_content_properties(&String::from_utf8_lossy(&content))
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            });
            chapter_bytes.push(content);
            spine_items.push(OpfItemref {
                idref: id,
                properties: None,
            });
        }

        // Add assets to manifest
        let assets: Vec<_> = book.list_assets().to_vec();
        for (i, asset_path) in assets.iter().enumerate() {
            let path_str = asset_path.to_string_lossy();
            manifest_items.push(OpfItem {
                id: format!("asset_{}", i),
                href: sanitize_path(&path_str),
                media_type: guess_media_type(&path_str),
                properties: Vec::new(),
            });
        }

        // 4. Build titlepage. Apple Books / Kindle only render a cover *page*
        // in the reading flow when a spine-positioned cover doc exists; the
        // manifest `properties="cover-image"` alone only drives the library
        // thumbnail. Cover image dimensions come from a JPEG SOF / PNG IHDR
        // probe of the actual asset bytes — `viewBox` collapses without them.
        let cover_id = find_cover_manifest_id(book.metadata(), &manifest_items);
        if let Some(cid) = &cover_id
            && let Some(item) = manifest_items.iter_mut().find(|i| &i.id == cid)
        {
            item.properties.push("cover-image".to_string());
        }
        // The source may already ship a cover page in the spine — a
        // calibre-lineage EPUB's `titlepage.xhtml` is the same SVG wrapper
        // `build_titlepage` emits. Synthesizing on top of it puts two cover
        // pages in the reading flow, both rendering the same image. Reuse the
        // source's page and skip synthesis (detected structurally by
        // `is_cover_only_document`, not any in-page marker): passthrough keeps
        // the source's own bytes, and dropping a spine document instead would
        // orphan every link into it.
        let source_cover_page: Option<String> = book
            .metadata()
            .cover_image
            .as_deref()
            .and_then(|cover| {
                chapter_bytes
                    .iter()
                    .position(|b| is_cover_only_document(&String::from_utf8_lossy(b), cover))
            })
            .and_then(|i| {
                let id = format!("chapter_{}", i);
                manifest_items
                    .iter()
                    .find(|item| item.id == id)
                    .map(|item| item.href.clone())
            });

        let titlepage_xhtml = match (&source_cover_page, &cover_id) {
            (Some(_), _) => None,
            (None, Some(cid)) => {
                let cover_item = manifest_items.iter().find(|i| &i.id == cid);
                cover_item.and_then(|item| {
                    let bytes = book.load_asset(std::path::Path::new(&item.href)).ok()?;
                    let (w, h) = crate::util::extract_image_dimensions(&bytes)?;
                    Some(build_titlepage(&item.href, Some((w, h))))
                })
            }
            (None, None) => None,
        };

        // Where the package's cover references point: the source's own page
        // when it has one, else the page synthesized just above.
        let cover_page_href: Option<String> = source_cover_page
            .clone()
            .or_else(|| titlepage_xhtml.as_ref().map(|_| "cover.xhtml".to_string()));
        if let Some(xhtml) = &titlepage_xhtml {
            manifest_items.insert(
                0,
                OpfItem {
                    id: "titlepage".to_string(),
                    href: "cover.xhtml".to_string(),
                    media_type: "application/xhtml+xml".to_string(),
                    properties: opf::xhtml_content_properties(xhtml)
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                },
            );
            spine_items.insert(
                0,
                OpfItemref {
                    idref: "titlepage".to_string(),
                    properties: None,
                },
            );
        }

        // 5. Write content.opf. The guide keeps its single cover reference
        // (readers use it to open on the cover page).
        let mut guide: Vec<OpfGuideRef> = Vec::new();
        if let Some(href) = &cover_page_href {
            opf::repoint_cover_guide(&mut guide, href);
        }
        let opf = opf::emit_opf(&OpfPackage {
            metadata: build_opf_metadata(book.metadata(), false, cover_id, None),
            manifest: manifest_items,
            spine: spine_items,
            guide,
        });
        zip.start_file("OEBPS/content.opf", deflated)
            .map_err(io_error)?;
        zip.write_all(opf.as_bytes())?;

        // 6a. Write nav.xhtml (EPUB 3 navigation document). Passthrough TOC
        // hrefs are already file paths — no resolution pass; entries keep
        // their source order (sources ship reading-ordered TOCs). Landmarks
        // reuse the OPF guide-type vocabulary; the emitter maps to EPUB 3.
        let toc_points = toc_to_navpoints(book.toc(), &|href| Some(href.to_string()));
        let nav_landmarks: Vec<OpfGuideRef> = book
            .landmarks()
            .iter()
            .map(|lm| OpfGuideRef {
                guide_type: opf::landmark_guide_type(lm.landmark_type).to_string(),
                title: lm.label.clone(),
                href: lm.href.clone(),
            })
            .collect();
        let toc_fallback = cover_page_href.clone().or_else(|| {
            book.spine()
                .first()
                .and_then(|e| book.source_id(e.id))
                .map(sanitize_path)
        });
        let nav = nav::emit_nav(&nav::NavDoc {
            title: &book.metadata().title,
            language: &book.metadata().language,
            toc: &toc_points,
            toc_fallback_href: toc_fallback.as_deref(),
            page_list: &[],
            landmarks: &nav_landmarks,
        });
        zip.start_file("OEBPS/nav.xhtml", deflated)
            .map_err(io_error)?;
        zip.write_all(nav.as_bytes())?;

        // 6b. Write toc.ncx (legacy fallback for EPUB 2 readers)
        let ncx = nav::emit_ncx(&nav::NcxDoc {
            title: &book.metadata().title,
            identifier: &book.metadata().identifier,
            toc: &toc_points,
            toc_fallback_href: toc_fallback.as_deref(),
        });
        zip.start_file("OEBPS/toc.ncx", deflated)
            .map_err(io_error)?;
        zip.write_all(ncx.as_bytes())?;

        // 6c. Write cover.xhtml when a cover was found.
        if let Some(ref xhtml) = titlepage_xhtml {
            zip.start_file("OEBPS/cover.xhtml", deflated)
                .map_err(io_error)?;
            zip.write_all(xhtml.as_bytes())?;
        }

        // 7. Write chapters (bytes already loaded for the manifest scan),
        // normalized to EPUB 3 conformance — passthrough preserves source bytes,
        // and pre-EPUB-3 source content (e.g. a Sigil-authored book carried
        // through AZW3: XHTML 1.1 DOCTYPE, empty <title>) is otherwise emitted
        // verbatim and fails epubcheck. Rendered content is untouched.
        //
        // Steps 7 and 8 are one `resources` phase to the caller: both deflate a
        // container entry, they cost the same per byte, and on an image-heavy
        // book they are together the whole of the export's wall time. Counting
        // them separately would run the bar to full twice. The nav write above
        // is microseconds on this route (passthrough hrefs need no resolution),
        // so it reports nothing and the phase order stays the one every EPUB
        // export promises: content → resources → finalize.
        let entries = spine.len() + assets.len();
        let mut written = 0usize;
        let book_title = book.metadata().title.clone();
        for (entry, content) in spine.iter().zip(&chapter_bytes) {
            let source_path = book
                .source_id(entry.id)
                .unwrap_or("unknown.xhtml")
                .to_string();
            let zip_path = format!("OEBPS/{}", sanitize_path(&source_path));
            let doc = normalize_passthrough_xhtml(content, &book_title);

            zip.start_file(&zip_path, deflated).map_err(io_error)?;
            zip.write_all(&doc)?;
            written += 1;
            on_progress("resources", written, entries, "Writing files");
        }

        // 8. Write assets
        for asset_path in &assets {
            let content = book.load_asset(asset_path)?;
            let zip_path = format!("OEBPS/{}", sanitize_path(&asset_path.to_string_lossy()));

            zip.start_file(&zip_path, deflated).map_err(io_error)?;
            zip.write_all(&content)?;
            written += 1;
            on_progress("resources", written, entries, "Writing files");
        }

        on_progress("finalize", 1, 1, "Packaging");
        zip.finish().map_err(io_error)?;
        Ok(())
    }

    /// Export with normalized content: the IR pipeline's clean, consistent
    /// output. Reports coarse phase progress to `on_progress` as
    /// `content → resources → nav → finalize`.
    ///
    /// `load_assets` transcodes inline, giving the manifest each image's
    /// post-transcode MIME. Each asset enters the container as it comes off
    /// the transcode; one chunk of image bytes is resident.
    fn export_normalized<W: Write + Seek>(
        &self,
        book: &mut Book,
        writer: &mut W,
        on_progress: &dyn Fn(&str, usize, usize, &str),
    ) -> io::Result<()> {
        let opts = ContainerOptions::new(&self.config);
        let mut zip = ZipWriter::new(writer);
        start_container(&mut zip, &opts)?;
        let package = {
            let mut sink = ZipAssets {
                zip: &mut zip,
                opts: &opts,
            };
            // A shipped container never carries source element ids.
            build_package_into(book, PackageOptions::container(), &mut sink, on_progress)?
        };
        finish_container(&mut zip, &package, &opts)?;
        zip.finish().map_err(io_error)?;
        Ok(())
    }

    /// Write a built package into an EPUB container.
    ///
    /// Each member is byte-identical to calibre's; the zip entry order is
    /// [`finish_container`]'s, and a reader takes entries by name.
    pub fn write_package<W: Write + Seek>(
        &self,
        package: &EpubPackage,
        writer: &mut W,
    ) -> io::Result<()> {
        let opts = ContainerOptions::new(&self.config);
        let mut zip = ZipWriter::new(writer);
        start_container(&mut zip, &opts)?;
        for asset in &package.assets {
            // A described-only package names assets it never loaded. Writing
            // it would produce a container whose manifest promises files it
            // does not hold, so refuse rather than ship one.
            let Some(bytes) = &asset.bytes else {
                return Err(io::Error::other(format!(
                    "cannot write a container from a package built to describe assets: \
                     {} has no bytes",
                    asset.href
                )));
            };
            write_asset(&mut zip, &opts, asset, bytes)?;
        }
        finish_container(&mut zip, package, &opts)?;
        zip.finish().map_err(io_error)?;
        Ok(())
    }
}

/// The zip entry options an EPUB container is written with.
struct ContainerOptions {
    stored: SimpleFileOptions,
    deflated: SimpleFileOptions,
}

impl ContainerOptions {
    fn new(config: &EpubConfig) -> Self {
        let compression_level = config.compression_level.unwrap_or(6);
        Self {
            stored: SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            deflated: SimpleFileOptions::default()
                .compression_method(CompressionMethod::Deflated)
                .compression_level(Some(compression_level as i64)),
        }
    }
}

/// The two entries every EPUB opens with.
fn start_container<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    opts: &ContainerOptions,
) -> io::Result<()> {
    // mimetype must be first and uncompressed.
    zip.start_file("mimetype", opts.stored).map_err(io_error)?;
    zip.write_all(b"application/epub+zip")?;

    zip.start_file("META-INF/container.xml", opts.deflated)
        .map_err(io_error)?;
    zip.write_all(CONTAINER_XML)?;
    Ok(())
}

/// One asset entry. An image type carrying its own compression is `Stored`:
/// deflate over it gains <5% at ~10-15 ms per MB, most of an image-heavy
/// book's export cost.
fn write_asset<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    opts: &ContainerOptions,
    asset: &PackageAsset,
    bytes: &[u8],
) -> io::Result<()> {
    let method = if is_precompressed_mime(&asset.media_type) {
        opts.stored
    } else {
        opts.deflated
    };
    let zip_path = format!("OEBPS/{}", sanitize_path(&asset.href));
    zip.start_file(&zip_path, method).map_err(io_error)?;
    zip.write_all(bytes)?;
    Ok(())
}

/// Everything after the assets, in manifest registration order: package
/// document, navigation, stylesheet, spine documents, titlepage last.
fn finish_container<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    package: &EpubPackage,
    opts: &ContainerOptions,
) -> io::Result<()> {
    zip.start_file("OEBPS/content.opf", opts.deflated)
        .map_err(io_error)?;
    zip.write_all(package.opf.as_bytes())?;

    zip.start_file("OEBPS/nav.xhtml", opts.deflated)
        .map_err(io_error)?;
    zip.write_all(package.nav.as_bytes())?;

    zip.start_file("OEBPS/toc.ncx", opts.deflated)
        .map_err(io_error)?;
    zip.write_all(package.ncx.as_bytes())?;

    if !package.css.is_empty() {
        zip.start_file("OEBPS/style.css", opts.deflated)
            .map_err(io_error)?;
        zip.write_all(package.css.as_bytes())?;
    }

    for (i, doc) in package.documents.iter().enumerate() {
        // ONE cover in the shipped container: the titlepage below is the
        // page that fills a foreign reader's viewport, so the source's own
        // cover page goes (see `EpubPackage::redundant_cover`).
        if Some(i) == package.redundant_cover {
            continue;
        }
        let zip_path = format!("OEBPS/{}", doc.href);
        zip.start_file(&zip_path, opts.deflated).map_err(io_error)?;
        zip.write_all(doc.xhtml.as_bytes())?;
    }

    if let Some(xhtml) = &package.titlepage {
        zip.start_file("OEBPS/cover.xhtml", opts.deflated)
            .map_err(io_error)?;
        zip.write_all(xhtml.as_bytes())?;
    }
    Ok(())
}

/// The [`AssetSink`] that writes each asset straight into the container.
struct ZipAssets<'a, W: Write + Seek> {
    zip: &'a mut ZipWriter<W>,
    opts: &'a ContainerOptions,
}

impl<W: Write + Seek> AssetSink for ZipAssets<'_, W> {
    fn take(&mut self, asset: &PackageAsset, bytes: Vec<u8>) -> io::Result<()> {
        write_asset(self.zip, self.opts, asset, &bytes)
    }
}

/// Build a normalized book into EPUB shape without writing a container,
/// keeping every asset's bytes on the package it returns.
///
/// The whole normalized pipeline — chapter synthesis, style unification,
/// asset transcode, package document, navigation — stopping short of the zip.
/// Reports coarse phase progress as `(phase_key, current, total, label)`.
pub fn build_package(
    book: &mut Book,
    opts: PackageOptions,
    on_progress: &dyn Fn(&str, usize, usize, &str),
) -> io::Result<EpubPackage> {
    let mut collected = CollectAssets::default();
    let mut package = build_package_into(book, opts, &mut collected, on_progress)?;
    for (asset, bytes) in package.assets.iter_mut().zip(collected.0) {
        asset.bytes = Some(bytes);
    }
    Ok(package)
}

/// [`build_package`] with each asset's bytes sent to `sink`. The assets on the
/// returned package carry their href, media type and pixel size with
/// `bytes: None`.
pub fn build_package_into(
    book: &mut Book,
    opts: PackageOptions,
    sink: &mut dyn AssetSink,
    on_progress: &dyn Fn(&str, usize, usize, &str),
) -> io::Result<EpubPackage> {
    /// Assets described from the importer's declared manifest, in the given
    /// order. An entry the manifest doesn't mention still appears — with the
    /// media type guessed from its name — so the list stays a faithful
    /// account of what the documents reference.
    fn return_described(
        paths: Vec<std::path::PathBuf>,
        declared: &HashMap<String, crate::import::AssetInfo>,
    ) -> Vec<PackageAsset> {
        paths
            .into_iter()
            .map(|p| {
                let href = p.to_string_lossy().to_string();
                match declared.get(&href) {
                    Some(info) => PackageAsset {
                        href,
                        media_type: info.media_type.clone(),
                        width: info.width,
                        height: info.height,
                        bytes: None,
                    },
                    None => PackageAsset {
                        media_type: guess_media_type(&href),
                        href,
                        width: None,
                        height: None,
                        bytes: None,
                    },
                }
            })
            .collect()
    }

    /// Describe one loaded asset and hand its bytes to `sink`. `None` for
    /// empty `bytes`: a manifest entry with no file behind it is a container
    /// defect (RSC-001).
    fn describe_loaded(
        href: String,
        bytes: Vec<u8>,
        sink: &mut dyn AssetSink,
    ) -> io::Result<Option<PackageAsset>> {
        if bytes.is_empty() {
            return Ok(None);
        }
        // Sniff first: post-transcode bytes are the truth (a JPEG-XR that
        // failed to decode passes through as image/jxr despite its .jpg name).
        // Extension guess covers unsniffable formats (SVG).
        let media_type = sniff_image_media_type(&bytes)
            .map(str::to_string)
            .unwrap_or_else(|| guess_media_type(&href));
        let (width, height) = match crate::util::extract_image_dimensions(&bytes) {
            Some((w, h)) => (Some(w), Some(h)),
            None => (None, None),
        };
        let asset = PackageAsset {
            href,
            media_type,
            width,
            height,
            bytes: None,
        };
        sink.take(&asset, bytes)?;
        Ok(Some(asset))
    }

    {
        use self::normalize::normalize_book_with;
        let source_elements = opts.source_elements;

        // Normalize the book content — for KFX this triggers the lazy per-chapter
        // storyline→IR parse (the heaviest step after image transcode).
        on_progress("content", 0, 1, "Building chapters");
        let content = normalize_book_with(book, source_elements)?;
        let spine: Vec<_> = book.spine().to_vec();

        // Output filename per chapter, derived from the chapter's source id
        // (for KFX: the section name). The `{section}.xhtml` shape and the
        // `-N` collision suffix are calibre's, kept because a reader's stored
        // links and bookmarks are written against these names.
        let mut chapter_files =
            chapter_filenames(content.chapters.iter().map(|c| c.source_path.as_str()));

        // The KFX ships its cover as an image-only section that the SVG
        // `cover.xhtml` page emitted below renders a second time. Report the
        // overlap (`EpubPackage::redundant_cover`) rather than resolving it —
        // but the OPF and the navigation this builds describe a *container*,
        // which ships one cover, so those passes skip the index. Gated to when
        // a cover page is actually emitted (non-FXL + a cover image, which is
        // always in the manifest when set). Calibre drops the
        // same section on the identical bytes (1:1).
        let cover_section_idx: Option<usize> = if !book.metadata().fixed_layout {
            book.metadata().cover_image.as_deref().and_then(|cover| {
                content
                    .chapters
                    .iter()
                    .position(|c| is_cover_only_document(&c.document, cover))
            })
        } else {
            None
        };
        // Each document keeps its own name; `chapter_files` is the *navigation*
        // view of the same list. Nav/landmark targets that resolved to the
        // suppressed section (the KFX's toc/text landmarks can point at the
        // cover) resolve to the SVG cover page in its place — so
        // `resolve_nav_href` maps its id to `cover.xhtml`, not a file the
        // container doesn't hold. Same remap on calibre (via
        // `element_id_to_filename`), so nav/ncx stay 1:1.
        let document_files = chapter_files.clone();
        if let Some(idx) = cover_section_idx {
            chapter_files[idx] = "cover.xhtml".to_string();
        }

        // Build manifest, in calibre's registration order —
        // images (canonical index order), stylesheet, chapters, titlepage
        // last — with ids derived from filenames (`opf::make_manifest_id`),
        // so a KFX conversion produces the same package document on both
        // engines. Manifest ids therefore depend on registration order:
        // don't reorder these blocks.
        let mut taken_ids: HashSet<String> = HashSet::new();
        let next_id = |taken: &mut HashSet<String>, name: &str| -> String {
            let id = opf::make_manifest_id(name, |candidate| taken.contains(candidate));
            taken.insert(id.clone());
            id
        };
        let mut manifest_items: Vec<OpfItem> = Vec::new();
        let mut spine_items: Vec<OpfItemref> = Vec::new();

        // Assets first. When the importer declares an authoritative bundle
        // (KFX: the canonical image index shared with calibre —
        // deterministic order, exported filenames, cover included even when
        // no chapter references it inline), use it verbatim; the KFX
        // JPEG-XR→JPEG transcode runs across cores (`load_assets` →
        // `parallel_map`). Otherwise fall back to the assets the normalized
        // content references, sorted for stable ordering, force-including the
        // cover image (it may be referenced by metadata only).
        let asset_bytes: Vec<PackageAsset> = if let Some(asset_list) = book.bundled_assets() {
            // Fixed-layout books bundle a full page-thumbnail set the
            // reading order never references; ship only the images the
            // emitted pages use (plus the cover), pruned BEFORE the bulk
            // load so thumbnails are never transcoded (calibre's
            // route's `retain_referenced_images`).
            let asset_list: Vec<std::path::PathBuf> = if book.metadata().fixed_layout {
                let cover = book.metadata().cover_image.clone();
                asset_list
                    .into_iter()
                    .filter(|p| {
                        // Bind `&str` explicitly: `name.as_ref()` alone is
                        // ambiguous whenever a dependency contributes another
                        // `AsRef`/`Borrow` impl for `Cow<str>`/`String`, which
                        // makes the build depend on which crates happen to be
                        // in the graph.
                        let name: &str = &p.to_string_lossy();
                        content.assets.contains(name) || Some(name) == cover.as_deref()
                    })
                    .collect()
            } else {
                asset_list
            };
            // Transcode in chunks so the progress bar moves through the long
            // pole (image decode) instead of sitting on one opaque step; each
            // chunk still transcodes across cores. ~2 items per worker keeps
            // straggler idle time negligible — calibre's
            // `resources` chunking. Byte output is identical to a single bulk
            // load (`parallel_map` preserves input order within each chunk and
            // chunks concatenate in order).
            let total = asset_list.len();
            if total > 0 {
                on_progress("resources", 0, total, "Decoding images");
            }
            if opts.assets == Assets::Describe {
                // Describe without reading: the declared manifest carries the
                // media type and pixel size, so this costs nothing even for a
                // book whose images would take seconds to decode.
                let declared: HashMap<String, crate::import::AssetInfo> = book
                    .asset_manifest()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|a| (a.path.to_string_lossy().to_string(), a))
                    .collect();
                return_described(asset_list, &declared)
            } else {
                let chunk_size = (crate::util::resolve_workers(book.max_workers()) * 2).max(1);
                let mut loaded: Vec<PackageAsset> = Vec::with_capacity(total);
                let mut done = 0usize;
                for chunk in asset_list.chunks(chunk_size) {
                    for (path, bytes) in chunk.iter().zip(book.load_assets(chunk)) {
                        let href = path.to_string_lossy().to_string();
                        if let Some(asset) = describe_loaded(href, bytes.unwrap_or_default(), sink)?
                        {
                            loaded.push(asset);
                        }
                    }
                    done += chunk.len();
                    on_progress("resources", done, total, "Decoding images");
                }
                loaded
            }
        } else if opts.assets == Assets::Describe {
            let mut asset_paths: Vec<String> = content.assets.iter().cloned().collect();
            asset_paths.sort();
            let declared: HashMap<String, crate::import::AssetInfo> = book
                .asset_manifest()
                .unwrap_or_default()
                .into_iter()
                .map(|a| (a.path.to_string_lossy().to_string(), a))
                .collect();
            return_described(
                asset_paths
                    .into_iter()
                    .map(std::path::PathBuf::from)
                    .collect(),
                &declared,
            )
        } else {
            let mut asset_paths: Vec<String> = content.assets.iter().cloned().collect();
            asset_paths.sort();
            if let Some(cover) = book.metadata().cover_image.as_ref()
                && !cover.trim().is_empty()
                && !asset_paths.iter().any(|a| a == cover)
            {
                asset_paths.push(cover.clone());
            }
            let mut loaded: Vec<PackageAsset> = Vec::with_capacity(asset_paths.len());
            for path in asset_paths {
                let bytes = book
                    .load_asset(std::path::Path::new(&path))
                    .unwrap_or_default();
                if let Some(asset) = describe_loaded(path, bytes, sink)? {
                    loaded.push(asset);
                }
            }
            loaded
        };
        for asset in &asset_bytes {
            let href = sanitize_path(&asset.href);
            manifest_items.push(OpfItem {
                id: next_id(&mut taken_ids, &href),
                href,
                media_type: asset.media_type.clone(),
                properties: Vec::new(),
            });
        }

        // Stylesheet.
        if !content.css.is_empty() {
            manifest_items.push(OpfItem {
                id: next_id(&mut taken_ids, "style.css"),
                href: "style.css".to_string(),
                media_type: "text/css".to_string(),
                properties: Vec::new(),
            });
        }

        // Chapters. Spine `properties` carry the FXL page-spread pairing
        // when the importer set one.
        for (i, chapter) in content.chapters.iter().enumerate() {
            if Some(i) == cover_section_idx {
                continue;
            }
            let id = next_id(&mut taken_ids, &chapter_files[i]);
            manifest_items.push(OpfItem {
                id: id.clone(),
                href: chapter_files[i].clone(),
                media_type: "application/xhtml+xml".to_string(),
                properties: opf::xhtml_content_properties(&chapter.document)
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            });
            spine_items.push(OpfItemref {
                idref: id,
                properties: spine
                    .get(i)
                    .and_then(|e| e.page_spread)
                    .map(|p| p.opf_property().to_string()),
            });
        }

        // 4. Build titlepage from the cover (same rationale as export_raw —
        // Apple Books needs a spine-positioned cover doc to render the cover
        // page in the reading flow). The `viewBox` reads the cover's pixel size
        // off its `PackageAsset`, at no second `load_asset`.
        let cover_id = find_cover_manifest_id(book.metadata(), &manifest_items);
        if let Some(cid) = &cover_id
            && let Some(item) = manifest_items.iter_mut().find(|i| &i.id == cid)
        {
            item.properties.push("cover-image".to_string());
        }
        let titlepage_xhtml = if book.metadata().fixed_layout {
            // Fixed-layout: the first spine page already IS the cover; a
            // titlepage would duplicate it and break the spread pairing.
            None
        } else if let Some(ref cid) = cover_id {
            manifest_items.iter().find(|i| &i.id == cid).map(|item| {
                // Dims are optional — the shared builder falls back to a bare
                // <img> when the cover's pixel size can't be probed. Emit the
                // page whenever a cover exists (matching calibre),
                // so suppressing the KFX cover section below never leaves the
                // book coverless.
                let dims = asset_bytes
                    .iter()
                    .find(|a| sanitize_path(&a.href) == item.href)
                    .and_then(|a| Some((a.width?, a.height?)));
                build_titlepage(&item.href, dims)
            })
        } else {
            None
        };
        if let Some(xhtml) = &titlepage_xhtml {
            let id = next_id(&mut taken_ids, "cover.xhtml");
            manifest_items.push(OpfItem {
                id: id.clone(),
                href: "cover.xhtml".to_string(),
                media_type: "application/xhtml+xml".to_string(),
                properties: opf::xhtml_content_properties(xhtml)
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            });
            spine_items.insert(
                0,
                OpfItemref {
                    idref: id,
                    properties: None,
                },
            );
        }

        // 5. Resolve navigation targets (TOC tree, page list, landmarks).
        // Hrefs arrive as `#eid[:offset]` placeholders and resolve to chapter
        // files through the importer's anchor index — built from chapters the
        // normalize pass already cached, so this costs one DFS per chapter,
        // not a re-parse. Unresolvable landmarks and page-list entries are
        // dropped (never emit a dangling reference); TOC entries keep their
        // label with an empty href, like calibre.
        on_progress("nav", 0, 1, "Writing navigation");
        if !book.toc().is_empty() || !book.page_list().is_empty() || !book.landmarks().is_empty() {
            let mut anchor_chapters = Vec::with_capacity(spine.len());
            for (i, entry) in spine.iter().enumerate() {
                if Some(i) == cover_section_idx {
                    continue;
                }
                anchor_chapters.push((entry.id, book.load_chapter_cached(entry.id)?));
            }
            book.index_anchors(&anchor_chapters);
        }
        let chapter_pos: HashMap<crate::import::ChapterId, usize> =
            spine.iter().enumerate().map(|(i, e)| (e.id, i)).collect();
        // Fragment rules (in `resolve_nav_href`) match calibre:
        // TOC and guide/landmark entries carry the anchor registered at the
        // target position whenever one exists; the page list only keeps a
        // fragment that was actually stamped into content (a page break on an
        // already-anchored chapter start registers a name content never
        // stamps — the bare chapter link is where the page starts anyway, and
        // a dangling `#page-…` would trip epubcheck RSC-012).
        let mut toc_points = toc_to_navpoints(book.toc(), &|href| {
            resolve_nav_href(
                book,
                href,
                &chapter_pos,
                &chapter_files,
                false,
                cover_section_idx,
            )
        });
        // EPUB 3 requires the toc nav in reading order (epubcheck NAV-011);
        // calibre sorts, so sort identically. Kept for the *container* only —
        // the package hands a renderer the book's own order (see `package_toc`).
        let authored_toc = toc_points.clone();
        let file_rank: HashMap<String, usize> = chapter_files
            .iter()
            .enumerate()
            .map(|(i, f)| (f.clone(), i))
            .collect();
        nav::sort_toc_reading_order(&mut toc_points, &file_rank);

        // Page list: flat, kept in page order. The unlabelled book-start
        // sentinel Amazon ships ("Untitled" after the importer's label
        // fallback) and entries whose target never resolves are dropped —
        // calibre's `extract_page_list` rules.
        let page_points: Vec<NavPoint> = book
            .page_list()
            .iter()
            .filter(|e| e.title != "Untitled")
            .filter_map(|e| {
                Some(NavPoint {
                    label: e.title.clone(),
                    href: resolve_nav_href(
                        book,
                        &e.href,
                        &chapter_pos,
                        &chapter_files,
                        true,
                        cover_section_idx,
                    )?,
                    children: Vec::new(),
                })
            })
            .collect();

        let mut guide: Vec<OpfGuideRef> = Vec::new();
        for lm in book.landmarks() {
            let Some(href) = resolve_nav_href(
                book,
                &lm.href,
                &chapter_pos,
                &chapter_files,
                false,
                cover_section_idx,
            ) else {
                continue;
            };
            guide.push(OpfGuideRef {
                guide_type: opf::landmark_guide_type(lm.landmark_type).to_string(),
                title: lm.label.clone(),
                href,
            });
        }
        if titlepage_xhtml.is_some() {
            opf::repoint_cover_guide(&mut guide, "cover.xhtml");
        }
        // Fixed-layout `original-resolution` fallback for sources with
        // per-page viewports only: the most common page size (the cover is
        // often sized differently from the content pages). Deterministic
        // tie-break (count, then size) — calibre's HashMap
        // `max_by_key` is tie-order-unstable; don't copy that.
        let derived_resolution = {
            let mut counts: Vec<((u32, u32), usize)> = Vec::new();
            for entry in &spine {
                if let Some(vp) = entry.viewport {
                    match counts.iter_mut().find(|(k, _)| *k == vp) {
                        Some((_, n)) => *n += 1,
                        None => counts.push((vp, 1)),
                    }
                }
            }
            counts
                .into_iter()
                .max_by_key(|&(vp, n)| (n, vp))
                .map(|(vp, _)| vp)
        };
        // Packaging: OPF + nav/ncx (image bytes are already transcoded, so
        // this is serialization, not the long pole).
        on_progress("finalize", 1, 1, "Packaging");
        // A KFX source's metadata mirrors calibre's curated field set (see
        // `build_opf_metadata`), so the package document keeps calibre's
        // shape. The guide is cloned into the package — the nav doc's
        // landmarks render from the same entries.
        let opf = opf::emit_opf(&OpfPackage {
            metadata: build_opf_metadata(
                book.metadata(),
                book.requires_normalized_export(),
                cover_id,
                derived_resolution,
            ),
            manifest: manifest_items,
            spine: spine_items,
            guide: guide.clone(),
        });

        // nav.xhtml (EPUB 3 navigation document). The empty-TOC fallback
        // points at the first spine document — the titlepage when one was
        // synthesized, like calibre.
        let toc_fallback = if titlepage_xhtml.is_some() {
            Some("cover.xhtml")
        } else {
            chapter_files.first().map(String::as_str)
        };
        let nav = nav::emit_nav(&nav::NavDoc {
            title: &book.metadata().title,
            language: &book.metadata().language,
            toc: &toc_points,
            toc_fallback_href: toc_fallback,
            page_list: &page_points,
            landmarks: &guide,
        });

        // toc.ncx (legacy fallback for EPUB 2 readers).
        let ncx = nav::emit_ncx(&nav::NcxDoc {
            title: &book.metadata().title,
            identifier: &book.metadata().identifier,
            toc: &toc_points,
            toc_fallback_href: toc_fallback,
        });

        // The TOC the package carries names `documents`; the one just emitted
        // into nav/ncx names container files, and the two differ exactly where
        // the source's cover section was remapped onto the synthesized
        // `cover.xhtml`. A renderer keeps that section (see `redundant_cover`)
        // and never holds a `cover.xhtml`, so the container's view would give
        // it a TOC row that navigates nowhere. Fragments come back here too:
        // they were dropped because the SVG wrapper carries no ids, which is
        // not true of the page the renderer actually shows.
        //
        // It also keeps the source's OWN entry order, unsorted. The
        // reading-order sort above buys epubcheck's NAV-011 for a shipped
        // container; a renderer instead has to show the chapter list the book
        // declares, because that is the list every other reader of the same
        // source shows. The two only diverge when a publisher's spine
        // disagrees with its own navigation — a collection whose spine is
        // ordered by filename rather than by chapter, say — and there the sort
        // scrambles the chapter list rather than fixing it.
        let mut package_toc = match cover_section_idx {
            None => authored_toc,
            Some(_) => toc_to_navpoints(book.toc(), &|href| {
                resolve_nav_href(book, href, &chapter_pos, &document_files, false, None)
            }),
        };
        if let Some(cover) = cover_nav_point(book, cover_section_idx, &document_files, &package_toc)
        {
            package_toc.insert(0, cover);
        }

        // Every spine document the source produced, under its own name. The
        // titlepage stays separate (spine position 0, but written last). When a
        // cover section was dropped for the synthesized `cover.xhtml`, remap the
        // in-content links that still point at it (baked by normalize before the
        // drop was known) — the nav did this via `resolve_nav_href`; the content
        // docs need it too, or the link dangles (RSC-007).
        let dropped_cover_href = cover_section_idx.map(|idx| document_files[idx].clone());
        let documents = content
            .chapters
            .into_iter()
            .enumerate()
            .map(|(i, chapter)| {
                let xhtml = match &dropped_cover_href {
                    Some(dropped) if Some(i) != cover_section_idx => {
                        remap_dropped_cover_links(&chapter.document, dropped)
                    }
                    _ => chapter.document,
                };
                PackageDocument {
                    href: document_files[i].clone(),
                    xhtml,
                    spread: spine
                        .get(i)
                        .and_then(|e| e.page_spread)
                        .map(|p| p.opf_property().to_string()),
                    viewport: spine.get(i).and_then(|e| e.viewport),
                }
            })
            .collect();

        Ok(EpubPackage {
            documents,
            titlepage: titlepage_xhtml,
            redundant_cover: cover_section_idx,
            opf,
            nav,
            ncx,
            css: content.css,
            assets: asset_bytes,
            toc: package_toc,
            writing_mode: content.writing_mode,
        })
    }
}

/// Media types whose bytes are already compressed; running deflate over them
/// gains <5% while consuming ~10-15 ms per MB.
fn is_precompressed_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/jpeg"
            | "image/png"
            | "image/webp"
            | "image/gif"
            | "image/jxr"
            | "image/vnd.ms-photo"
    )
}

/// Convert zip error to io error.
fn io_error<E: std::error::Error + Send + Sync + 'static>(e: E) -> io::Error {
    io::Error::other(e)
}

/// Container.xml template.
const CONTAINER_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"#;

/// Find the manifest item whose asset corresponds to `metadata.cover_image`.
///
/// `cover_image` carries the value populated by the importer: for an EPUB
/// source this is typically a path like `images/cover.jpg`; for a KFX source
/// the exported cover filename (`cover.jpeg`). Manifest hrefs always end
/// with the asset's raw path, so a suffix match covers both cases without
/// format-aware normalization.
fn find_cover_manifest_id(
    metadata: &crate::model::Metadata,
    manifest: &[OpfItem],
) -> Option<String> {
    let cover = metadata.cover_image.as_ref()?;
    let cover_trim = cover.trim_start_matches('/');
    if cover_trim.is_empty() {
        return None;
    }
    manifest
        .iter()
        .find(|item| item.href == cover_trim || item.href.ends_with(cover_trim))
        .map(|item| item.id.clone())
}

/// True when `html` is an image-only document whose sole image renders
/// `cover_href` and which carries no visible text of its own — i.e. a cover
/// page, in either of the two shapes one ships in: a bare `<img>` (how KFX
/// ships its cover section) or an SVG `<image>` wrapper (how a calibre-lineage
/// EPUB ships its `titlepage.xhtml`).
///
/// A publisher EPUB (Kobo-verified) ships ONE cover page, so both export routes
/// use this to avoid emitting two — but they resolve the duplicate in opposite
/// directions, because the surviving page differs:
///
/// - Normalized: the source section is a bare `<img>` that inherits the
///   reader's body margins, so it is dropped and the synthesized full-page SVG
///   survives. Both KFX→EPUB routes feed this the identical section bytes, so
///   they drop the same section and stay byte-for-byte 1:1.
/// - Raw: the source's page is already the SVG shape, so it survives and
///   synthesis is skipped — passthrough keeps the source's bytes rather than
///   substituting an equivalent page and orphaning links into it.
///
/// The reader drops the SVG page and keeps the section instead (it renders the
/// KFX natively and sizes the image itself) — a third resolution, correct for
/// its renderer.
pub(crate) fn is_cover_only_document(html: &str, cover_href: &str) -> bool {
    // A full-bleed image page (the shape predicate is shared with TOC repair,
    // which uses it to find volume starts) whose image is *the* cover.
    match page_shape::single_image_source(html) {
        Some(src) => page_shape::basename(src) == page_shape::basename(cover_href),
        None => false,
    }
}

/// Normalize a passthrough content document to EPUB 3 conformance without
/// touching rendered content: replace a non-`<!DOCTYPE html>` DOCTYPE (pre-EPUB-3
/// source keeps its old one → epubcheck `HTM-004`) and fill an empty `<title>`
/// (`RSC-005`). Returns the input unchanged when it is already conformant or is
/// not UTF-8 text.
fn normalize_passthrough_xhtml(content: &[u8], fallback_title: &str) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(content) else {
        return content.to_vec();
    };
    let mut owned: Option<String> = None;
    if let Some((start, end)) = doctype_span(text)
        && !text[start..end].eq_ignore_ascii_case("<!DOCTYPE html>")
    {
        let mut s = text.to_string();
        s.replace_range(start..end, "<!DOCTYPE html>");
        owned = Some(s);
    }
    if let Some(fixed) = fill_empty_title(owned.as_deref().unwrap_or(text), fallback_title) {
        owned = Some(fixed);
    }
    owned
        .map(String::into_bytes)
        .unwrap_or_else(|| content.to_vec())
}

/// Byte span of the document's `<!DOCTYPE …>` declaration, or `None` if it has
/// none. A DOCTYPE sits at the top of the document; match the two casings real
/// content uses (`<!DOCTYPE` / `<!doctype`) directly, without slicing at a byte
/// offset that could split a multi-byte character.
fn doctype_span(s: &str) -> Option<(usize, usize)> {
    let start = s.find("<!DOCTYPE").or_else(|| s.find("<!doctype"))?;
    let end = start + s[start..].find('>')? + 1;
    Some((start, end))
}

/// If the first `<title>` is empty (or self-closing), return the document with it
/// filled by `fallback`; `None` when the title is already non-empty or absent.
fn fill_empty_title(s: &str, fallback: &str) -> Option<String> {
    let open = s.find("<title")?;
    let gt = open + s[open..].find('>')?;
    let self_closing = s.as_bytes()[gt - 1] == b'/';
    let elem_end = if self_closing {
        gt + 1
    } else {
        let cs = gt + 1;
        let ce = cs + s[cs..].find("</title>")?;
        if !s[cs..ce].trim().is_empty() {
            return None; // already non-empty
        }
        ce + "</title>".len()
    };
    let mut out = String::with_capacity(s.len() + fallback.len() + 16);
    out.push_str(&s[..open]);
    out.push_str("<title>");
    out.push_str(&crate::formats::epub::edit::escape_text(fallback));
    out.push_str("</title>");
    out.push_str(&s[elem_end..]);
    Some(out)
}

/// Build the OPF `<metadata>` block from the book's metadata.
///
/// With `normalized` set (KFX sources), the field set follows calibre's:
/// `dc:date` gets the `issue_date` ISO formatting, creators carry positional
/// per-author sort keys, and the fields calibre never emits (description,
/// subjects, rights, contributors, collection) are omitted. Other sources keep
/// the full field set.
fn build_opf_metadata(
    md: &crate::model::Metadata,
    normalized: bool,
    cover_manifest_id: Option<String>,
    derived_resolution: Option<(u32, u32)>,
) -> OpfMetadata {
    // Positional per-creator sort keys (shared with calibre via
    // `creator_file_as_keys`).
    let file_as_keys = opf::creator_file_as_keys(&md.authors, &md.author_sorts);
    let creators = md
        .authors
        .iter()
        .zip(&file_as_keys)
        .map(|(author, file_as)| OpfCreator {
            name: author.clone(),
            role: Some("aut".to_string()),
            file_as: Some(file_as.clone()),
        })
        .collect();

    let (contributors, description, subjects, rights, collection) = if normalized {
        (Vec::new(), None, Vec::new(), None, None)
    } else {
        (
            md.contributors
                .iter()
                .map(|c| OpfCreator {
                    name: c.name.clone(),
                    role: c.role.clone(),
                    file_as: c.file_as.clone(),
                })
                .collect(),
            md.description.clone(),
            md.subjects.clone(),
            md.rights.clone(),
            md.collection.as_ref().map(|c| OpfCollection {
                name: c.name.clone(),
                collection_type: c.collection_type.clone(),
                position: c.position,
            }),
        )
    };

    let date = if normalized {
        md.date.as_deref().map(opf::format_opf_date)
    } else {
        md.date.clone()
    };

    // Fixed-layout: a declared doc-level viewport doubles as the EBPAJ
    // viewport meta and the KF8 `original-resolution` twin. A source with
    // per-page viewports only (KFX) declares no EBPAJ viewport and falls
    // back to the derived modal page size for `original-resolution`.
    let fixed_layout = md.fixed_layout.then(|| OpfFixedLayout {
        rendition_spread: md.rendition_spread.clone(),
        ebpaj_viewport: md.default_viewport,
        original_resolution: md.default_viewport.or(derived_resolution),
        book_type: md.book_type.clone(),
    });

    OpfMetadata {
        title: md.title.clone(),
        title_file_as: md.title_sort.clone(),
        creators,
        contributors,
        language: md.language.clone(),
        identifier: md.identifier.clone(),
        asin: md.asin.clone(),
        modified: crate::util::time_now_iso8601_utc(),
        date,
        publisher: md.publisher.clone(),
        description,
        subjects,
        rights,
        collection,
        cover_manifest_id,
        primary_writing_mode: md.primary_writing_mode.clone(),
        page_progression_direction: md.page_progression_direction.clone(),
        fixed_layout,
    }
}

/// Convert the model TOC tree to the shared emitter's [`NavPoint`] tree.
/// `resolve` maps a model href to the emitted document href — identity for
/// the passthrough path (source hrefs are file paths), anchor-index
/// resolution for the normalized path. Entries whose target doesn't resolve
/// keep their label with an empty href, matching calibre.
fn toc_to_navpoints(
    entries: &[TocEntry],
    resolve: &dyn Fn(&str) -> Option<String>,
) -> Vec<NavPoint> {
    entries
        .iter()
        .map(|entry| NavPoint {
            label: entry.title.clone(),
            href: resolve(&entry.href).unwrap_or_default(),
            children: toc_to_navpoints(&entry.children, resolve),
        })
        .collect()
}

/// The cover row a renderer's chapter list needs, or `None` when it already has
/// one.
///
/// A publisher's chapter list often opens with a "Cover" entry, but plenty of
/// books leave the cover out of it and record the page only in the landmarks —
/// and a chapter list mined from a book's own Contents page never has one at
/// all, because a Contents page links neither the cover nor itself. A Kindle
/// composes the two views and shows a Cover row either way; a renderer reading
/// this package should reach the same page, so the row is synthesized here when
/// the list doesn't already reach the cover document.
///
/// The row reaches the *renderer's* view only. The nav doc and NCX describe a
/// container, which ships its own `cover.xhtml` under its own landmarks; the
/// source's own TOC stays as the publisher wrote it.
fn cover_nav_point(
    book: &Book,
    cover_section_idx: Option<usize>,
    document_files: &[String],
    toc: &[NavPoint],
) -> Option<NavPoint> {
    // The renderer keeps the section the container drops (`redundant_cover`);
    // with no such section there is no cover page to send it to.
    let href = document_files.get(cover_section_idx?)?;
    cover_row(cover_label(book), href, toc)
}

/// What the book calls its cover, falling back to the landmark type's own name —
/// some containers mark the page with a placeholder label instead of a real one.
fn cover_label(book: &Book) -> String {
    book.landmarks()
        .iter()
        .find(|l| l.landmark_type == LandmarkType::Cover)
        .map(|l| l.label.trim())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| LandmarkType::Cover.default_label())
        .to_string()
}

/// The row to put in front of `toc`, or `None` when it already reaches `href`.
fn cover_row(label: String, href: &str, toc: &[NavPoint]) -> Option<NavPoint> {
    if toc_reaches(toc, href) {
        return None;
    }
    Some(NavPoint {
        label,
        href: href.to_string(),
        children: Vec::new(),
    })
}

/// Whether any entry in the tree already sends the reader into `document`.
/// Compared without fragments: a row aimed at an anchor inside the cover page is
/// still a row that opens the cover page.
fn toc_reaches(toc: &[NavPoint], document: &str) -> bool {
    toc.iter()
        .any(|p| p.href.split('#').next() == Some(document) || toc_reaches(&p.children, document))
}

// `cover.xhtml` comes from the shared calibre-shaped builder
// (`export::titlepage`), the same one calibre ships.
use self::titlepage::build_titlepage;

/// Sanitize a path for use in ZIP (remove leading slashes, normalize).
fn sanitize_path(path: &str) -> String {
    path.trim_start_matches('/')
        .replace('\\', "/")
        .replace("//", "/")
}

/// Output filename for each normalized chapter: `{source_id}.xhtml` (for KFX,
/// the section name), unique via a `-N` suffix on collision. Both rules are
/// calibre's (`push_book_part`), kept so spine filenames stay stable for
/// anything already linking to them.
/// Chapters without a usable source id fall back to positional names. Takes
/// the chapters' source paths (also used pre-synthesis by the normalize
/// pass's link resolver, which must know target filenames before any
/// document exists). Shared with the AZW3 exporter, which names its KF8 spine
/// files identically so `normalize_book`'s resolved links match the spine.
pub(crate) fn chapter_filenames<'a, I>(source_paths: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let source_paths: Vec<&str> = source_paths.into_iter().collect();
    let mut names: Vec<String> = Vec::with_capacity(source_paths.len());
    for (i, sp) in source_paths.iter().enumerate() {
        let source = sanitize_path(sp);
        let base = if source.is_empty() || source == "unknown.xhtml" {
            format!("chapter_{i}")
        } else {
            source.trim_end_matches(".xhtml").to_string()
        };
        let mut candidate = format!("{base}.xhtml");
        if names.contains(&candidate) {
            let mut n = 1;
            loop {
                let cand = format!("{base}-{n}.xhtml");
                if !names.contains(&cand) {
                    candidate = cand;
                    break;
                }
                n += 1;
            }
        }
        names.push(candidate);
    }
    names
}

/// Resolve an IR navigation href (`#eid[:offset]` placeholder) to a
/// `file#frag` target — or a bare `file` when no fragment applies — against
/// the spine's chapter files. `chapter_pos` maps each spine chapter's
/// [`ChapterId`](crate::import::ChapterId) to its index in `chapter_files`.
/// `require_stamped` drops a fragment content never actually stamped (the
/// page-list rule); `false` keeps any registered anchor (the TOC / landmark
/// rule). Returns `None` when the target doesn't resolve to a spine chapter.
///
/// `book.index_anchors` must have run over the spine chapters first. Shared by
/// the EPUB normalized nav/landmark resolution and the AZW3 normalized
/// exporter so both resolve identical targets — a bare `#eid` otherwise
/// collapses every TOC entry onto its chapter start.
pub(crate) fn resolve_nav_href(
    book: &Book,
    href: &str,
    chapter_pos: &HashMap<crate::import::ChapterId, usize>,
    chapter_files: &[String],
    require_stamped: bool,
    dropped_idx: Option<usize>,
) -> Option<String> {
    let AnchorTarget::Internal(target) =
        book.resolve_toc_href(crate::import::ChapterId(0), href)?
    else {
        return None;
    };
    let pos = *chapter_pos.get(&target.chapter)?;
    let file = chapter_files.get(pos).cloned()?;
    // A target inside the dropped cover section keeps its file (remapped to the
    // synthesized cover page) but loses its fragment: that page is a bare SVG
    // wrapper carrying no ids, so any `#…` on it dangles (epubcheck RSC-012).
    // A second route to a dangling fragment, independent of the `stamped` test
    // below: there the anchor never reached content, here its content is gone.
    if Some(pos) == dropped_idx {
        return Some(file);
    }
    match book.nav_fragment(href) {
        Some((frag, stamped)) if !require_stamped || stamped => Some(format!("{file}#{frag}")),
        _ => Some(file),
    }
}

/// Rewrite in-content `<a href>` links that point at the dropped cover section
/// (`dropped_href`, e.g. `c0.xhtml`) to the synthesized `cover.xhtml`, dropping
/// any fragment — that page is a bare SVG wrapper with no ids. The nav path does
/// the same remap in [`resolve_nav_href`] via `dropped_idx`; this covers the
/// links the normalize pass baked into content *before* the cover section was
/// known to be dropped, which would otherwise resolve to a file the container
/// never emits (epubcheck RSC-007). Matches only when the href value is exactly
/// the dropped file or that file plus a `#fragment`, so a longer name sharing the
/// prefix (`c0.xhtml2`) is left untouched.
fn remap_dropped_cover_links(html: &str, dropped_href: &str) -> String {
    let needle = format!("href=\"{dropped_href}");
    if !html.contains(&needle) {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find(&needle) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + needle.len()..];
        match after.find('"') {
            // The href value is `after[..close]`; remap only a bare match or a
            // pure `#fragment` tail.
            Some(close) if after[..close].is_empty() || after[..close].starts_with('#') => {
                out.push_str("href=\"cover.xhtml\"");
                rest = &after[close + 1..];
            }
            // A different file sharing the prefix — keep the matched text verbatim.
            _ => {
                out.push_str(&rest[pos..pos + needle.len()]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Guess media type from file extension. Shared with the AZW3 exporter, which
/// keys images/fonts/CSS resource routing on the same guesses.
pub(crate) fn guess_media_type(path: &str) -> String {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "xhtml" | "html" | "htm" => "application/xhtml+xml".to_string(),
        "css" => "text/css".to_string(),
        "js" => "application/javascript".to_string(),
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "png" => "image/png".to_string(),
        "gif" => "image/gif".to_string(),
        "svg" => "image/svg+xml".to_string(),
        "webp" => "image/webp".to_string(),
        "bmp" => "image/bmp".to_string(),
        "jxr" => "image/jxr".to_string(),
        "ttf" => "font/ttf".to_string(),
        "otf" => "font/otf".to_string(),
        "woff" => "font/woff".to_string(),
        "woff2" => "font/woff2".to_string(),
        "ncx" => "application/x-dtbncx+xml".to_string(),
        "opf" => "application/oebps-package+xml".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

/// Sniff an image MIME type from the leading bytes. Returns `None` if the
/// signature doesn't match a known image format — covers KFX assets stored
/// without a file extension (e.g. `e20`, `eF`) so the OPF can advertise the
/// right `media-type` and Apple Books / ADE will pick the cover up.
fn sniff_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 3 && &bytes[..3] == b"\xFF\xD8\xFF" {
        return Some("image/jpeg");
    }
    if bytes.len() >= 8 && &bytes[..8] == b"\x89PNG\r\n\x1a\n" {
        return Some("image/png");
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.len() >= 2 && &bytes[..2] == b"BM" {
        return Some("image/bmp");
    }
    // JPEG-XR / WMP container: II-BC magic.
    if bytes.len() >= 3 && bytes[..3] == [0x49, 0x49, 0xBC] {
        return Some("image/jxr");
    }
    // SVG: text format, sniff by the first non-whitespace char + signature
    if bytes.len() >= 5 {
        let head = std::str::from_utf8(&bytes[..bytes.len().min(256)])
            .unwrap_or("")
            .trim_start();
        if head.starts_with("<svg") || head.starts_with("<?xml") && head.contains("<svg") {
            return Some("image/svg+xml");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chapter_filenames_use_source_ids_with_port_dedup() {
        // KFX section names (no extension) get `.xhtml`; collisions get `-N`
        // (the `push_book_part` rule); placeholders fall back positionally.
        assert_eq!(
            chapter_filenames(["secA", "secA", "secA", "unknown.xhtml", "secB.xhtml"]),
            vec![
                "secA.xhtml",
                "secA-1.xhtml",
                "secA-2.xhtml",
                "chapter_3.xhtml",
                "secB.xhtml"
            ]
        );
    }

    #[test]
    fn remap_dropped_cover_links_remaps_only_exact_matches() {
        // A link into the dropped cover section (bare or `#frag`) becomes a bare
        // `cover.xhtml`; a file that merely shares the prefix, or a different
        // file, is untouched.
        let html = concat!(
            r#"<a href="c0.xhtml#aYS">Cover</a> "#,
            r#"<a href="c0.xhtml">C</a> "#,
            r#"<a href="c0.xhtml2#x">Other</a> "#,
            r#"<a href="c1F.xhtml#y">Ch</a>"#,
        );
        let out = remap_dropped_cover_links(html, "c0.xhtml");
        assert!(
            out.contains(r#"<a href="cover.xhtml">Cover</a>"#),
            "frag dropped: {out}"
        );
        assert!(
            out.contains(r#"<a href="cover.xhtml">C</a>"#),
            "bare remapped: {out}"
        );
        assert!(
            out.contains(r#"<a href="c0.xhtml2#x">Other</a>"#),
            "prefix-share kept: {out}"
        );
        assert!(
            out.contains(r#"<a href="c1F.xhtml#y">Ch</a>"#),
            "other file kept: {out}"
        );
        // No-op when the dropped file is absent.
        assert_eq!(
            remap_dropped_cover_links("<p>x</p>", "c0.xhtml"),
            "<p>x</p>"
        );
    }

    #[test]
    fn a_renderers_toc_gains_a_cover_row_only_when_it_lacks_one() {
        let row = |label: &str, href: &str| NavPoint {
            label: label.into(),
            href: href.into(),
            children: Vec::new(),
        };

        // A chapter list that never reaches the cover page gains a row for it —
        // the case a list mined from a book's own Contents page always hits.
        let chapters = vec![row("Epigraph", "c0.xhtml"), row("1", "c1.xhtml")];
        assert_eq!(
            cover_row("Cover".into(), "cover-page.xhtml", &chapters).map(|p| p.href),
            Some("cover-page.xhtml".to_string())
        );

        // A publisher who already listed it gets nothing added — the row would
        // be the same page twice.
        let with_cover = vec![row("表紙", "cover-page.xhtml"), row("1", "c1.xhtml")];
        assert!(cover_row("Cover".into(), "cover-page.xhtml", &with_cover).is_none());

        // Including when the entry aims at an anchor inside the cover page, or
        // sits nested under another entry.
        let by_anchor = vec![row("Cover", "cover-page.xhtml#top")];
        assert!(cover_row("Cover".into(), "cover-page.xhtml", &by_anchor).is_none());
        let nested = vec![NavPoint {
            label: "Front Matter".into(),
            href: "fm.xhtml".into(),
            children: vec![row("Cover", "cover-page.xhtml")],
        }];
        assert!(cover_row("Cover".into(), "cover-page.xhtml", &nested).is_none());
    }

    #[test]
    fn passthrough_normalize_rewrites_doctype_and_fills_title() {
        let doc = "<?xml version=\"1.0\"?>\n\
            <!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.1//EN\" \"x.dtd\">\n\
            <html><head><title></title></head><body><p>hi</p></body></html>";
        let out =
            String::from_utf8(normalize_passthrough_xhtml(doc.as_bytes(), "My Book")).unwrap();
        assert!(out.contains("<!DOCTYPE html>") && !out.contains("XHTML 1.1"));
        assert!(
            out.contains("<title>My Book</title>"),
            "empty title filled: {out}"
        );
    }

    #[test]
    fn passthrough_normalize_is_a_noop_for_conformant_docs() {
        let doc = "<!DOCTYPE html>\n<html><head><title>Ch. 1</title></head><body>x</body></html>";
        assert_eq!(
            normalize_passthrough_xhtml(doc.as_bytes(), "Book"),
            doc.as_bytes(),
            "already-valid doc is byte-identical"
        );
    }

    #[test]
    fn passthrough_normalize_handles_multibyte_content_without_panicking() {
        // Regression: an earlier version sliced at byte 1024 to find the DOCTYPE,
        // which splits a multi-byte char in CJK content and panics.
        let filler = "字".repeat(2000); // ~6 KB of 3-byte chars, crosses byte 1024
        let doc = format!(
            "<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.1//EN\" \"x.dtd\">\n\
             <html><head><title></title></head><body><p>{filler}</p></body></html>"
        );
        let out = String::from_utf8(normalize_passthrough_xhtml(doc.as_bytes(), "书")).unwrap();
        assert!(out.contains("<!DOCTYPE html>") && !out.contains("XHTML 1.1"));
        assert!(out.contains("<title>书</title>") && out.contains(&filler));
    }

    #[test]
    fn passthrough_normalize_fills_self_closing_title_and_escapes() {
        let doc = "<!DOCTYPE html>\n<html><head><title/></head><body>x</body></html>";
        let out = String::from_utf8(normalize_passthrough_xhtml(doc.as_bytes(), "A & B")).unwrap();
        assert!(
            out.contains("<title>A &amp; B</title>"),
            "filled + escaped: {out}"
        );
    }

    #[test]
    fn cover_only_document_matches_both_page_shapes() {
        // The bare-`<img>` shape (a KFX cover section) and the SVG-wrapper
        // shape (a calibre-lineage EPUB's `titlepage.xhtml`) both count: each
        // is a cover page, and emitting one alongside a synthesized page gives
        // the reader two covers.
        let raster = r#"<html><body><div><img src="cover.jpeg" alt=""/></div></body></html>"#;
        let vector = r#"<html><body><div><svg viewBox="0 0 60 90">
            <image width="60" height="90" xlink:href="images/cover.jpeg"/>
            </svg></div></body></html>"#;
        assert!(is_cover_only_document(raster, "cover.jpeg"));
        assert!(is_cover_only_document(vector, "cover.jpeg"));

        // A different image is not the cover page.
        assert!(!is_cover_only_document(raster, "other.jpeg"));
        // Visible text means it is a content page that happens to open with
        // the cover art, not a cover page.
        let with_text = r#"<html><body><img src="cover.jpeg"/><p>Chapter One</p></body></html>"#;
        assert!(!is_cover_only_document(with_text, "cover.jpeg"));
        // Two images is a gallery, not a cover.
        let two = r#"<html><body><img src="cover.jpeg"/><img src="cover.jpeg"/></body></html>"#;
        assert!(!is_cover_only_document(two, "cover.jpeg"));
        // <title> text sits outside the body and must not count as visible.
        let titled =
            r#"<html><head><title>Cover</title></head><body><img src="cover.jpeg"/></body></html>"#;
        assert!(is_cover_only_document(titled, "cover.jpeg"));
    }

    #[test]
    fn test_sanitize_path() {
        assert_eq!(sanitize_path("/path/to/file.xhtml"), "path/to/file.xhtml");
        assert_eq!(sanitize_path("path\\to\\file.xhtml"), "path/to/file.xhtml");
    }

    #[test]
    fn test_guess_media_type() {
        assert_eq!(guess_media_type("file.xhtml"), "application/xhtml+xml");
        assert_eq!(guess_media_type("style.css"), "text/css");
        assert_eq!(guess_media_type("image.jpg"), "image/jpeg");
        assert_eq!(guess_media_type("font.woff2"), "font/woff2");
    }

    #[test]
    fn kfx_export_with_progress_emits_phases_in_order() {
        use std::cell::RefCell;
        // KFX → EPUB (the normalized path). The IR route transcodes images
        // inline before nav, so the emission order is
        // content → resources → nav → finalize (calibre's is
        // nav → resources — it defers image bytes). The fixture ships a cover
        // image, so all four phases fire.
        let mut book = crate::Book::open("tests/fixtures/[太宰 治] 人間失格.kfx").unwrap();
        let phases = RefCell::new(Vec::<String>::new());
        let mut sink = Vec::new();
        book.export_with_progress(
            crate::Format::Epub,
            &mut std::io::Cursor::new(&mut sink),
            &|phase, cur, total, _label| {
                assert!(cur <= total, "{phase}: {cur}/{total}");
                let mut p = phases.borrow_mut();
                if p.last().map(String::as_str) != Some(phase) {
                    p.push(phase.to_string());
                }
            },
        )
        .unwrap();
        let seen = phases.into_inner();
        let order = ["content", "resources", "nav", "finalize"];
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
    }
}
