//! EPUB exporter.
//!
//! Creates EPUB 2/3 files from Book structures using passthrough for content.

use std::collections::{HashMap, HashSet};
use std::io::{self, Seek, Write};
use std::path::Path;

use zip::CompressionMethod;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::model::{AnchorTarget, Book, Landmark, LandmarkType, TocEntry};

use super::Exporter;
use super::opf::{
    self, OpfCollection, OpfCreator, OpfFixedLayout, OpfGuideRef, OpfItem, OpfItemref, OpfMetadata,
    OpfPackage,
};

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

        // Add chapters to manifest
        for (i, entry) in spine.iter().enumerate() {
            let source_path = book.source_id(entry.id).unwrap_or("unknown.xhtml");
            let id = format!("chapter_{}", i);
            manifest_items.push(OpfItem {
                id: id.clone(),
                href: sanitize_path(source_path),
                media_type: "application/xhtml+xml".to_string(),
                properties: Vec::new(),
            });
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
                Some(build_titlepage(&item.href, w, h))
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
            metadata: build_opf_metadata(book.metadata(), false, cover_id),
            manifest: manifest_items,
            spine: spine_items,
            guide,
        });
        zip.start_file("OEBPS/content.opf", deflated)
            .map_err(io_error)?;
        zip.write_all(opf.as_bytes())?;

        // 6a. Write nav.xhtml (EPUB 3 navigation document)
        let nav_fallback = book
            .spine()
            .first()
            .and_then(|e| book.source_id(e.id))
            .map(sanitize_path)
            .unwrap_or_else(|| "chapter_0.xhtml".to_string());
        let nav = generate_nav(book.metadata(), book.toc(), book.landmarks(), &nav_fallback);
        zip.start_file("OEBPS/nav.xhtml", deflated)
            .map_err(io_error)?;
        zip.write_all(nav.as_bytes())?;

        // 6b. Write toc.ncx (legacy fallback for EPUB 2 readers)
        let ncx = generate_ncx(book.metadata(), book.toc());
        zip.start_file("OEBPS/toc.ncx", deflated)
            .map_err(io_error)?;
        zip.write_all(ncx.as_bytes())?;

        // 6c. Write titlepage.xhtml when a cover was found.
        if let Some(ref xhtml) = titlepage_xhtml {
            zip.start_file("OEBPS/titlepage.xhtml", deflated)
                .map_err(io_error)?;
            zip.write_all(xhtml.as_bytes())?;
        }

        // 7. Write chapters
        for entry in &spine {
            let source_path = book
                .source_id(entry.id)
                .unwrap_or("unknown.xhtml")
                .to_string();
            let content = book.load_raw(entry.id)?;
            let zip_path = format!("OEBPS/{}", sanitize_path(&source_path));

            zip.start_file(&zip_path, deflated).map_err(io_error)?;
            zip.write_all(&content)?;
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
        use super::normalize::normalize_book;

        // Normalize the book content
        let content = normalize_book(book)?;
        let spine: Vec<_> = book.spine().to_vec();

        // Output filename per chapter, derived from the chapter's source id
        // (for KFX: the section name). Must match the mechanical
        // `kfx_to_epub` route byte-for-byte — same `{section}.xhtml` shape,
        // same `-N` collision suffix (`content::push_book_part`) — so the
        // two routes' trees can converge to identical.
        let chapter_files = chapter_filenames(&content.chapters);

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
        let titlepage_xhtml = if let Some(ref cid) = cover_id {
            let cover_item = manifest_items.iter().find(|i| &i.id == cid);
            cover_item.and_then(|item| {
                let bytes = asset_bytes
                    .iter()
                    .find(|(p, _)| sanitize_path(p) == item.href)
                    .map(|(_, b)| b.as_slice())?;
                let (w, h) = crate::util::extract_image_dimensions(bytes)?;
                Some(build_titlepage(&item.href, w, h))
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

        // 5. Write content.opf. Landmarks become the EPUB 2 `<guide>`; their
        // hrefs arrive as `#eid[:offset]` placeholders and resolve to chapter
        // files through the importer's anchor index — built from chapters the
        // normalize pass already cached, so this costs one DFS per chapter,
        // not a re-parse. Unresolvable landmarks are dropped (never emit a
        // dangling guide reference).
        let mut guide: Vec<OpfGuideRef> = Vec::new();
        if !book.landmarks().is_empty() {
            let mut anchor_chapters = Vec::with_capacity(spine.len());
            for entry in &spine {
                anchor_chapters.push((entry.id, book.load_chapter_cached(entry.id)?));
            }
            book.index_anchors(&anchor_chapters);
            let chapter_pos: HashMap<crate::import::ChapterId, usize> =
                spine.iter().enumerate().map(|(i, e)| (e.id, i)).collect();
            for lm in book.landmarks() {
                let Some(AnchorTarget::Internal(target)) =
                    book.resolve_toc_href(crate::import::ChapterId(0), &lm.href)
                else {
                    continue;
                };
                let Some(&idx) = chapter_pos.get(&target.chapter) else {
                    continue;
                };
                guide.push(OpfGuideRef {
                    guide_type: opf::landmark_guide_type(lm.landmark_type).to_string(),
                    title: lm.label.clone(),
                    href: chapter_files[idx].clone(),
                });
            }
        }
        if titlepage_xhtml.is_some() {
            opf::repoint_cover_guide(&mut guide, "titlepage.xhtml");
        }
        // A KFX source's metadata mirrors the mechanical route's curated
        // field set (see `build_opf_metadata`), keeping the two KFX→EPUB
        // engines' package documents identical.
        let opf = opf::emit_opf(&OpfPackage {
            metadata: build_opf_metadata(
                book.metadata(),
                book.requires_normalized_export(),
                cover_id,
            ),
            manifest: manifest_items,
            spine: spine_items,
            guide,
        });
        zip.start_file("OEBPS/content.opf", deflated)
            .map_err(io_error)?;
        zip.write_all(opf.as_bytes())?;

        // 6a. Write nav.xhtml (EPUB 3 navigation document)
        let nav_fallback = chapter_files
            .first()
            .map(String::as_str)
            .unwrap_or("chapter_0.xhtml");
        let nav = generate_nav(book.metadata(), book.toc(), book.landmarks(), nav_fallback);
        zip.start_file("OEBPS/nav.xhtml", deflated)
            .map_err(io_error)?;
        zip.write_all(nav.as_bytes())?;

        // 6b. Write toc.ncx (legacy fallback for EPUB 2 readers)
        let ncx = generate_ncx(book.metadata(), book.toc());
        zip.start_file("OEBPS/toc.ncx", deflated)
            .map_err(io_error)?;
        zip.write_all(ncx.as_bytes())?;

        // 6c. Write titlepage.xhtml when a cover was found.
        if let Some(ref xhtml) = titlepage_xhtml {
            zip.start_file("OEBPS/titlepage.xhtml", deflated)
                .map_err(io_error)?;
            zip.write_all(xhtml.as_bytes())?;
        }

        // 7. Write unified stylesheet
        if !content.css.is_empty() {
            zip.start_file("OEBPS/style.css", deflated)
                .map_err(io_error)?;
            zip.write_all(content.css.as_bytes())?;
        }

        // 8. Write synthesized chapters
        for (i, chapter) in content.chapters.iter().enumerate() {
            let zip_path = format!("OEBPS/{}", chapter_files[i]);
            zip.start_file(&zip_path, deflated).map_err(io_error)?;
            zip.write_all(chapter.document.as_bytes())?;
        }

        // 9. Write assets (reuse the bytes we already loaded for MIME sniffing).
        for (asset_path, bytes) in &asset_bytes {
            if bytes.is_empty() {
                continue;
            }
            let zip_path = format!("OEBPS/{}", sanitize_path(asset_path));
            zip.start_file(&zip_path, deflated).map_err(io_error)?;
            zip.write_all(bytes)?;
        }

        zip.finish().map_err(io_error)?;
        Ok(())
    }
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
/// With `kfx_parity` set (KFX sources), the field set and value shapes match
/// the mechanical `kfx_to_epub` route exactly: `dc:date` gets the
/// `issue_date` ISO formatting, every creator shares one sort key, and the
/// fields that route never emits (description, subjects, rights,
/// contributors, collection) are omitted so both engines produce one package
/// document. Other sources keep the full field set.
fn build_opf_metadata(
    md: &crate::model::Metadata,
    kfx_parity: bool,
    cover_manifest_id: Option<String>,
) -> OpfMetadata {
    // One sort key for every creator: the author sort key when the source
    // declares one, else the joined author list so EPUB libraries still
    // sort multi-author books.
    let author_file_as = md
        .author_sort
        .clone()
        .unwrap_or_else(|| md.authors.join(" & "));
    let creators = md
        .authors
        .iter()
        .map(|author| OpfCreator {
            name: author.clone(),
            role: Some("aut".to_string()),
            file_as: Some(author_file_as.clone()),
        })
        .collect();

    let (contributors, description, subjects, rights, collection) = if kfx_parity {
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

    let date = if kfx_parity {
        md.date.as_deref().map(opf::format_opf_date)
    } else {
        md.date.clone()
    };

    // Fixed-layout: the source viewport doubles as the EBPAJ viewport meta
    // and the KF8 `original-resolution` twin.
    let fixed_layout = md.fixed_layout.then(|| OpfFixedLayout {
        rendition_spread: md.rendition_spread.clone(),
        ebpaj_viewport: md.default_viewport,
        original_resolution: md.default_viewport,
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

/// Generate `nav.xhtml`, the EPUB 3 navigation document.
///
/// Every EPUB 3 publication MUST contain exactly one navigation document
/// per the W3C EPUB 3.3 spec (NCX is a legacy fallback that does NOT
/// satisfy this requirement). Apple Books enforces the spec strictly and
/// rejects EPUB 3 packages missing this document.
///
/// Emits `<nav epub:type="toc">` from `Metadata::toc`, plus
/// `<nav epub:type="landmarks">` from `Metadata::landmarks` when any
/// landmarks are present.
fn generate_nav(
    metadata: &crate::model::Metadata,
    toc: &[TocEntry],
    landmarks: &[Landmark],
    first_chapter_href: &str,
) -> String {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str("<!DOCTYPE html>\n");
    let lang = if metadata.language.is_empty() {
        "en"
    } else {
        metadata.language.as_str()
    };
    s.push_str(&format!(
        "<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\" xml:lang=\"{}\" lang=\"{}\">\n",
        escape_xml(lang),
        escape_xml(lang),
    ));
    s.push_str("<head>\n");
    s.push_str("  <meta charset=\"utf-8\"/>\n");
    s.push_str("  <title>Navigation</title>\n");
    s.push_str("</head>\n<body>\n");

    // Table of contents.
    s.push_str("  <nav epub:type=\"toc\" id=\"toc\">\n");
    s.push_str("    <h1>Table of Contents</h1>\n");
    if toc.is_empty() {
        // Spec requires the nav to be non-empty; fall back to the title
        // pointing at the first content document. Empty TOC is unusual but
        // exists on minimal books.
        s.push_str(&format!(
            "    <ol>\n      <li><a href=\"{}\">",
            escape_xml(first_chapter_href)
        ));
        s.push_str(&escape_xml(if metadata.title.is_empty() {
            "Content"
        } else {
            metadata.title.as_str()
        }));
        s.push_str("</a></li>\n    </ol>\n");
    } else {
        write_nav_list(&mut s, toc, 2);
    }
    s.push_str("  </nav>\n");

    // Landmarks — hidden from rendered TOC view but used by readers for
    // "skip to start reading" / "go to cover" actions.
    if !landmarks.is_empty() {
        s.push_str("  <nav epub:type=\"landmarks\" id=\"landmarks\" hidden=\"\">\n");
        s.push_str("    <h2>Landmarks</h2>\n");
        s.push_str("    <ol>\n");
        for lm in landmarks {
            let epub_type = landmark_epub_type(lm.landmark_type);
            s.push_str(&format!(
                "      <li><a epub:type=\"{}\" href=\"{}\">{}</a></li>\n",
                epub_type,
                escape_xml(&lm.href),
                escape_xml(&lm.label),
            ));
        }
        s.push_str("    </ol>\n");
        s.push_str("  </nav>\n");
    }

    s.push_str("</body>\n</html>\n");
    s
}

/// Recursively write `<ol><li>...</li></ol>` for the TOC tree.
fn write_nav_list(s: &mut String, entries: &[TocEntry], indent: usize) {
    let pad = "  ".repeat(indent);
    s.push_str(&pad);
    s.push_str("<ol>\n");
    for entry in entries {
        s.push_str(&pad);
        s.push_str(&format!(
            "  <li><a href=\"{}\">{}</a>",
            escape_xml(&entry.href),
            escape_xml(&entry.title),
        ));
        if !entry.children.is_empty() {
            s.push('\n');
            write_nav_list(s, &entry.children, indent + 2);
            s.push_str(&pad);
            s.push_str("  </li>\n");
        } else {
            s.push_str("</li>\n");
        }
    }
    s.push_str(&pad);
    s.push_str("</ol>\n");
}

/// Map an internal LandmarkType to the EPUB 3 `epub:type` vocabulary.
fn landmark_epub_type(t: LandmarkType) -> &'static str {
    match t {
        LandmarkType::Cover => "cover",
        LandmarkType::TitlePage => "titlepage",
        LandmarkType::Toc => "toc",
        // EPUB 3.3 deprecated `start` in favor of `bodymatter`; lump
        // StartReading + BodyMatter into bodymatter — both denote where
        // the main content begins.
        LandmarkType::StartReading | LandmarkType::BodyMatter => "bodymatter",
        LandmarkType::FrontMatter => "frontmatter",
        LandmarkType::BackMatter => "backmatter",
        LandmarkType::Acknowledgements => "acknowledgments",
        LandmarkType::Bibliography => "bibliography",
        LandmarkType::Glossary => "glossary",
        LandmarkType::Index => "index",
        LandmarkType::Preface => "preface",
        LandmarkType::Endnotes => "endnotes",
        LandmarkType::Loi => "loi",
        LandmarkType::Lot => "lot",
    }
}

/// Generate toc.ncx from TOC entries.
fn generate_ncx(metadata: &crate::model::Metadata, toc: &[TocEntry]) -> String {
    let mut ncx = String::new();

    ncx.push_str(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE ncx PUBLIC "-//NISO//DTD ncx 2005-1//EN" "http://www.daisy.org/z3986/2005/ncx-2005-1.dtd">
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head>
    <meta name="dtb:uid" content=""#,
    );
    ncx.push_str(&escape_xml(&metadata.identifier));
    ncx.push_str(
        r#""/>
    <meta name="dtb:depth" content="1"/>
    <meta name="dtb:totalPageCount" content="0"/>
    <meta name="dtb:maxPageNumber" content="0"/>
  </head>
  <docTitle>
    <text>"#,
    );
    ncx.push_str(&escape_xml(&metadata.title));
    ncx.push_str(
        r#"</text>
  </docTitle>
  <navMap>
"#,
    );

    let mut play_order = 1;
    write_nav_points(&mut ncx, toc, &mut play_order, 2);

    ncx.push_str("  </navMap>\n</ncx>\n");
    ncx
}

/// Recursively write navPoint elements.
fn write_nav_points(ncx: &mut String, entries: &[TocEntry], play_order: &mut usize, indent: usize) {
    let indent_str = "  ".repeat(indent);

    for entry in entries {
        ncx.push_str(&format!(
            "{}<navPoint id=\"navPoint-{}\" playOrder=\"{}\">\n",
            indent_str, play_order, play_order
        ));
        ncx.push_str(&format!(
            "{}  <navLabel><text>{}</text></navLabel>\n",
            indent_str,
            escape_xml(&entry.title)
        ));
        ncx.push_str(&format!(
            "{}  <content src=\"{}\"/>\n",
            indent_str,
            escape_xml(&entry.href)
        ));

        *play_order += 1;

        if !entry.children.is_empty() {
            write_nav_points(ncx, &entry.children, play_order, indent + 1);
        }

        ncx.push_str(&format!("{}</navPoint>\n", indent_str));
    }
}

/// Escape XML special characters.
fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Calibre-shaped `titlepage.xhtml` — an SVG `viewBox` sized to the cover
/// image's pixel dimensions, with the cover JPEG/PNG referenced via
/// `xlink:href`. Renders full-bleed in Apple Books / Kindle because the
/// `viewBox` is self-contained CSS-wise (bypasses the reader's body margin
/// defaults a plain `<img>` would inherit). `<meta name="calibre:cover">`
/// flags this as the title page rather than first content page.
///
/// `cover_href` is the spine-doc-relative path to the cover image (e.g.
/// `images/cover.jpg`). `w` / `h` come from a JPEG SOF / PNG IHDR probe.
fn build_titlepage(cover_href: &str, w: u32, h: u32) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE html>\n\
         <html xmlns=\"http://www.w3.org/1999/xhtml\" xml:lang=\"en\">\n\
         <head>\n\
         <meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\"/>\n\
         <meta name=\"calibre:cover\" content=\"true\"/>\n\
         <title>Cover</title>\n\
         <style type=\"text/css\" title=\"override_css\">\n\
         @page {{padding: 0pt; margin:0pt}}\n\
         body {{ text-align: center; padding:0pt; margin: 0pt; }}\n\
         </style>\n\
         </head>\n\
         <body>\n\
         <div>\n\
         <svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" version=\"1.1\" width=\"100%\" height=\"100%\" viewBox=\"0 0 {w} {h}\" preserveAspectRatio=\"none\">\n\
         <image width=\"{w}\" height=\"{h}\" xlink:href=\"{href}\"/>\n\
         </svg>\n\
         </div>\n\
         </body>\n\
         </html>\n",
        w = w,
        h = h,
        href = escape_xml(cover_href),
    )
}

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
/// Chapters without a usable source id fall back to positional names.
fn chapter_filenames(chapters: &[super::normalize::ChapterContent]) -> Vec<String> {
    let mut names: Vec<String> = Vec::with_capacity(chapters.len());
    for (i, ch) in chapters.iter().enumerate() {
        let source = sanitize_path(&ch.source_path);
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

/// Guess media type from file extension.
fn guess_media_type(path: &str) -> String {
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

    fn chapter(source_path: &str) -> super::super::normalize::ChapterContent {
        super::super::normalize::ChapterContent {
            id: crate::import::ChapterId(0),
            source_path: source_path.to_string(),
            document: String::new(),
        }
    }

    #[test]
    fn chapter_filenames_use_source_ids_with_port_dedup() {
        // KFX section names (no extension) get `.xhtml`; collisions get `-N`
        // (the `push_book_part` rule); placeholders fall back positionally.
        let chapters = vec![
            chapter("secA"),
            chapter("secA"),
            chapter("secA"),
            chapter("unknown.xhtml"),
            chapter("secB.xhtml"),
        ];
        assert_eq!(
            chapter_filenames(&chapters),
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
    fn test_escape_xml() {
        assert_eq!(escape_xml("Hello & World"), "Hello &amp; World");
        assert_eq!(escape_xml("<tag>"), "&lt;tag&gt;");
        assert_eq!(escape_xml("\"quoted\""), "&quot;quoted&quot;");
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
