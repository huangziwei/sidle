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

pub use normalize::{ChapterContent, GlobalStylePool, NormalizedContent, normalize_book};

use std::collections::{HashMap, HashSet};
use std::io::{self, Seek, Write};
use std::path::Path;

use zip::CompressionMethod;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::model::{AnchorTarget, Book, TocEntry};

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

/// EPUB format exporter.
///
/// Creates standard EPUB files compatible with most e-readers.
///
/// # Example
///
/// ```no_run
/// use boko::Book;
/// use boko::export::{EpubExporter, Exporter};
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
        // Use normalized mode if explicitly requested OR if the source format requires it
        // (e.g., KFX raw content is binary Ion, not HTML)
        if self.config.normalize || book.requires_normalized_export() {
            self.export_normalized(book, writer)
        } else {
            self.export_raw(book, writer)
        }
    }
}

impl EpubExporter {
    /// Export with passthrough mode (preserves original HTML/CSS).
    fn export_raw<W: Write + Seek>(&self, book: &mut Book, writer: &mut W) -> io::Result<()> {
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
        let titlepage_xhtml = if let Some(ref cid) = cover_id {
            let cover_item = manifest_items.iter().find(|i| &i.id == cid);
            cover_item.and_then(|item| {
                let bytes = book.load_asset(std::path::Path::new(&item.href)).ok()?;
                let (w, h) = crate::util::extract_image_dimensions(&bytes)?;
                Some(build_titlepage(&item.href, Some((w, h))))
            })
        } else {
            None
        };
        if let Some(xhtml) = &titlepage_xhtml {
            manifest_items.insert(
                0,
                OpfItem {
                    id: "titlepage".to_string(),
                    href: "titlepage.xhtml".to_string(),
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
        if titlepage_xhtml.is_some() {
            opf::repoint_cover_guide(&mut guide, "titlepage.xhtml");
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
        let toc_fallback = if titlepage_xhtml.is_some() {
            Some("titlepage.xhtml".to_string())
        } else {
            book.spine()
                .first()
                .and_then(|e| book.source_id(e.id))
                .map(sanitize_path)
        };
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

        // 6c. Write titlepage.xhtml when a cover was found.
        if let Some(ref xhtml) = titlepage_xhtml {
            zip.start_file("OEBPS/titlepage.xhtml", deflated)
                .map_err(io_error)?;
            zip.write_all(xhtml.as_bytes())?;
        }

        // 7. Write chapters (bytes already loaded for the manifest scan).
        for (entry, content) in spine.iter().zip(&chapter_bytes) {
            let source_path = book
                .source_id(entry.id)
                .unwrap_or("unknown.xhtml")
                .to_string();
            let zip_path = format!("OEBPS/{}", sanitize_path(&source_path));

            zip.start_file(&zip_path, deflated).map_err(io_error)?;
            zip.write_all(content)?;
        }

        // 8. Write assets
        for asset_path in &assets {
            let content = book.load_asset(asset_path)?;
            let zip_path = format!("OEBPS/{}", sanitize_path(&asset_path.to_string_lossy()));

            zip.start_file(&zip_path, deflated).map_err(io_error)?;
            zip.write_all(&content)?;
        }

        zip.finish().map_err(io_error)?;
        Ok(())
    }

    /// Export with normalized content (IR pipeline produces clean, consistent output).
    fn export_normalized<W: Write + Seek>(
        &self,
        book: &mut Book,
        writer: &mut W,
    ) -> io::Result<()> {
        use self::normalize::normalize_book;

        // Normalize the book content
        let content = normalize_book(book)?;
        let spine: Vec<_> = book.spine().to_vec();

        // Output filename per chapter, derived from the chapter's source id
        // (for KFX: the section name). Must match the mechanical
        // `kfx_to_epub` route byte-for-byte — same `{section}.xhtml` shape,
        // same `-N` collision suffix (`content::push_book_part`) — so the
        // two routes' trees can converge to identical.
        let chapter_files =
            chapter_filenames(content.chapters.iter().map(|c| c.source_path.as_str()));

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

        // 3. Build manifest, in the mechanical route's registration order —
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
        // (KFX: the canonical image index shared with the mechanical route —
        // deterministic order, exported filenames, cover included even when
        // no chapter references it inline), use it verbatim; bytes load in
        // one bulk call so the KFX JPEG-XR→JPEG transcode runs across cores.
        // Otherwise fall back to the assets the normalized content
        // references, sorted for stable ordering, force-including the cover
        // image (it may be referenced by metadata only).
        let asset_bytes: Vec<(String, Vec<u8>)> = if let Some(asset_list) = book.bundled_assets() {
            // Fixed-layout books bundle a full page-thumbnail set the
            // reading order never references; ship only the images the
            // emitted pages use (plus the cover), pruned BEFORE the bulk
            // load so thumbnails are never transcoded (the mechanical
            // route's `retain_referenced_images`).
            let asset_list: Vec<std::path::PathBuf> = if book.metadata().fixed_layout {
                let cover = book.metadata().cover_image.clone();
                asset_list
                    .into_iter()
                    .filter(|p| {
                        let name = p.to_string_lossy();
                        content.assets.contains(name.as_ref())
                            || Some(name.as_ref()) == cover.as_deref()
                    })
                    .collect()
            } else {
                asset_list
            };
            asset_list
                .iter()
                .zip(book.load_assets(&asset_list))
                .map(|(path, bytes)| {
                    (
                        path.to_string_lossy().to_string(),
                        bytes.unwrap_or_default(),
                    )
                })
                .collect()
        } else {
            let mut asset_paths: Vec<String> = content.assets.iter().cloned().collect();
            asset_paths.sort();
            if let Some(cover) = book.metadata().cover_image.as_ref()
                && !cover.trim().is_empty()
                && !asset_paths.iter().any(|a| a == cover)
            {
                asset_paths.push(cover.clone());
            }
            asset_paths
                .into_iter()
                .map(|path| {
                    let bytes = book
                        .load_asset(std::path::Path::new(&path))
                        .unwrap_or_default();
                    (path, bytes)
                })
                .collect()
        };
        for (asset_path, bytes) in &asset_bytes {
            // Sniff first: post-transcode bytes are the truth (a JPEG-XR that
            // failed to decode passes through as image/jxr despite its .jpg
            // name). Extension guess covers unsniffable formats (SVG).
            let media_type = sniff_image_media_type(bytes)
                .map(str::to_string)
                .unwrap_or_else(|| guess_media_type(asset_path));
            let href = sanitize_path(asset_path);
            manifest_items.push(OpfItem {
                id: next_id(&mut taken_ids, &href),
                href,
                media_type,
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
        // page in the reading flow). Asset bytes are already pre-loaded above
        // for MIME sniffing, so we don't pay a second `load_asset`.
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
            let cover_item = manifest_items.iter().find(|i| &i.id == cid);
            cover_item.and_then(|item| {
                let bytes = asset_bytes
                    .iter()
                    .find(|(p, _)| sanitize_path(p) == item.href)
                    .map(|(_, b)| b.as_slice())?;
                let (w, h) = crate::util::extract_image_dimensions(bytes)?;
                Some(build_titlepage(&item.href, Some((w, h))))
            })
        } else {
            None
        };
        if let Some(xhtml) = &titlepage_xhtml {
            let id = next_id(&mut taken_ids, "titlepage.xhtml");
            manifest_items.push(OpfItem {
                id: id.clone(),
                href: "titlepage.xhtml".to_string(),
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
        // label with an empty href, like the mechanical route.
        if !book.toc().is_empty() || !book.page_list().is_empty() || !book.landmarks().is_empty() {
            let mut anchor_chapters = Vec::with_capacity(spine.len());
            for entry in &spine {
                anchor_chapters.push((entry.id, book.load_chapter_cached(entry.id)?));
            }
            book.index_anchors(&anchor_chapters);
        }
        let chapter_pos: HashMap<crate::import::ChapterId, usize> =
            spine.iter().enumerate().map(|(i, e)| (e.id, i)).collect();
        // Fragment rules (in `resolve_nav_href`) match the mechanical route:
        // TOC and guide/landmark entries carry the anchor registered at the
        // target position whenever one exists; the page list only keeps a
        // fragment that was actually stamped into content (a page break on an
        // already-anchored chapter start registers a name content never
        // stamps — the bare chapter link is where the page starts anyway, and
        // a dangling `#page-…` would trip epubcheck RSC-012).
        let mut toc_points = toc_to_navpoints(book.toc(), &|href| {
            resolve_nav_href(book, href, &chapter_pos, &chapter_files, false)
        });
        // EPUB 3 requires the toc nav in reading order (epubcheck NAV-011);
        // the mechanical route sorts, so sort identically.
        let file_rank: HashMap<String, usize> = chapter_files
            .iter()
            .enumerate()
            .map(|(i, f)| (f.clone(), i))
            .collect();
        nav::sort_toc_reading_order(&mut toc_points, &file_rank);

        // Page list: flat, kept in page order. The unlabelled book-start
        // sentinel Amazon ships ("Untitled" after the importer's label
        // fallback) and entries whose target never resolves are dropped —
        // the mechanical route's `extract_page_list` rules.
        let page_points: Vec<NavPoint> = book
            .page_list()
            .iter()
            .filter(|e| e.title != "Untitled")
            .filter_map(|e| {
                Some(NavPoint {
                    label: e.title.clone(),
                    href: resolve_nav_href(book, &e.href, &chapter_pos, &chapter_files, true)?,
                    children: Vec::new(),
                })
            })
            .collect();

        let mut guide: Vec<OpfGuideRef> = Vec::new();
        for lm in book.landmarks() {
            let Some(href) = resolve_nav_href(book, &lm.href, &chapter_pos, &chapter_files, false)
            else {
                continue;
            };
            guide.push(OpfGuideRef {
                guide_type: opf::landmark_guide_type(lm.landmark_type).to_string(),
                title: lm.label.clone(),
                href,
            });
        }
        if titlepage_xhtml.is_some() {
            opf::repoint_cover_guide(&mut guide, "titlepage.xhtml");
        }
        // Fixed-layout `original-resolution` fallback for sources with
        // per-page viewports only: the most common page size (the cover is
        // often sized differently from the content pages). Deterministic
        // tie-break (count, then size) — the mechanical route's HashMap
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
        // A KFX source's metadata mirrors the mechanical route's curated
        // field set (see `build_opf_metadata`), keeping the two KFX→EPUB
        // engines' package documents identical. The guide is cloned into the
        // package — the nav doc's landmarks render from the same entries.
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
        zip.start_file("OEBPS/content.opf", deflated)
            .map_err(io_error)?;
        zip.write_all(opf.as_bytes())?;

        // 6a. Write nav.xhtml (EPUB 3 navigation document). The empty-TOC
        // fallback points at the first spine document — the titlepage when
        // one was synthesized, like the mechanical route.
        let toc_fallback = if titlepage_xhtml.is_some() {
            Some("titlepage.xhtml")
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
        zip.start_file("OEBPS/nav.xhtml", deflated)
            .map_err(io_error)?;
        zip.write_all(nav.as_bytes())?;

        // 6b. Write toc.ncx (legacy fallback for EPUB 2 readers)
        let ncx = nav::emit_ncx(&nav::NcxDoc {
            title: &book.metadata().title,
            identifier: &book.metadata().identifier,
            toc: &toc_points,
            toc_fallback_href: toc_fallback,
        });
        zip.start_file("OEBPS/toc.ncx", deflated)
            .map_err(io_error)?;
        zip.write_all(ncx.as_bytes())?;

        // 7. Write the OEBPS payload in manifest registration order — assets,
        // stylesheet, chapters, titlepage last — the same file order the
        // mechanical route's `finalize` walks, so the two engines' containers
        // are byte-identical, not merely entry-identical. Already-compressed
        // image types are `Stored`: deflate over them gains <5% at
        // ~10-15 ms per MB, most of an image-heavy book's export cost.
        for (asset_path, bytes) in &asset_bytes {
            if bytes.is_empty() {
                continue;
            }
            let media_type = sniff_image_media_type(bytes)
                .map(str::to_string)
                .unwrap_or_else(|| guess_media_type(asset_path));
            let opts = if is_precompressed_mime(&media_type) {
                stored
            } else {
                deflated
            };
            let zip_path = format!("OEBPS/{}", sanitize_path(asset_path));
            zip.start_file(&zip_path, opts).map_err(io_error)?;
            zip.write_all(bytes)?;
        }

        if !content.css.is_empty() {
            zip.start_file("OEBPS/style.css", deflated)
                .map_err(io_error)?;
            zip.write_all(content.css.as_bytes())?;
        }

        for (i, chapter) in content.chapters.iter().enumerate() {
            let zip_path = format!("OEBPS/{}", chapter_files[i]);
            zip.start_file(&zip_path, deflated).map_err(io_error)?;
            zip.write_all(chapter.document.as_bytes())?;
        }

        if let Some(ref xhtml) = titlepage_xhtml {
            zip.start_file("OEBPS/titlepage.xhtml", deflated)
                .map_err(io_error)?;
            zip.write_all(xhtml.as_bytes())?;
        }

        zip.finish().map_err(io_error)?;
        Ok(())
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

/// Build the OPF `<metadata>` block from the book's metadata.
///
/// With `normalized` set (KFX sources), the field set and value shapes match
/// the mechanical `kfx_to_epub` route exactly: `dc:date` gets the
/// `issue_date` ISO formatting, creators carry positional per-author sort
/// keys, and the fields that route never emits (description, subjects,
/// rights, contributors, collection) are omitted so both engines produce one
/// package document. Other sources keep the full field set.
fn build_opf_metadata(
    md: &crate::model::Metadata,
    normalized: bool,
    cover_manifest_id: Option<String>,
    derived_resolution: Option<(u32, u32)>,
) -> OpfMetadata {
    // Positional per-creator sort keys (shared with the mechanical route via
    // `creator_file_as_keys`, so both engines emit one shape).
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
/// keep their label with an empty href, matching the mechanical route.
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

// `titlepage.xhtml` comes from the shared calibre-shaped builder
// (`export::titlepage`), the same one the mechanical KFX→EPUB route ships.
use self::titlepage::build_titlepage;

/// Sanitize a path for use in ZIP (remove leading slashes, normalize).
fn sanitize_path(path: &str) -> String {
    path.trim_start_matches('/')
        .replace('\\', "/")
        .replace("//", "/")
}

/// Output filename for each normalized chapter: `{source_id}.xhtml` (for KFX,
/// the section name), unique via a `-N` suffix on collision. Both rules match
/// the mechanical route's `kfx_to_epub::content::push_book_part` exactly —
/// the two kfx→epub engines must name spine files identically to converge.
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
/// exporter so both engines resolve identical targets — a bare `#eid`
/// otherwise collapses every TOC entry onto its chapter start.
pub(crate) fn resolve_nav_href(
    book: &Book,
    href: &str,
    chapter_pos: &HashMap<crate::import::ChapterId, usize>,
    chapter_files: &[String],
    require_stamped: bool,
) -> Option<String> {
    let AnchorTarget::Internal(target) =
        book.resolve_toc_href(crate::import::ChapterId(0), href)?
    else {
        return None;
    };
    let file = chapter_files
        .get(*chapter_pos.get(&target.chapter)?)
        .cloned()?;
    match book.nav_fragment(href) {
        Some((frag, stamped)) if !require_stamped || stamped => Some(format!("{file}#{frag}")),
        _ => Some(file),
    }
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
}
