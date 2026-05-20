//! EPUB exporter.
//!
//! Creates EPUB 2/3 files from Book structures using passthrough for content.

use std::collections::HashMap;
use std::io::{self, Seek, Write};
use std::path::Path;

use zip::CompressionMethod;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::model::{Book, Landmark, LandmarkType, TocEntry};

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

        // 3. Collect content info for manifest
        let spine: Vec<_> = book.spine().to_vec();
        let mut manifest_items: Vec<ManifestItem> = Vec::new();
        let mut spine_refs: Vec<String> = Vec::new();

        // Add chapters to manifest
        for (i, entry) in spine.iter().enumerate() {
            let source_path = book.source_id(entry.id).unwrap_or("unknown.xhtml");
            let filename = format!("chapter_{}.xhtml", i);
            let id = format!("chapter_{}", i);

            manifest_items.push(ManifestItem {
                id: id.clone(),
                href: filename,
                media_type: "application/xhtml+xml".to_string(),
            });
            spine_refs.push(id);

            // Store original path for content writing
            manifest_items.last_mut().unwrap().href =
                format!("OEBPS/{}", sanitize_path(source_path));
        }

        // Add assets to manifest
        let assets: Vec<_> = book.list_assets().to_vec();
        let mut asset_map: HashMap<String, String> = HashMap::new();

        for (i, asset_path) in assets.iter().enumerate() {
            let path_str = asset_path.to_string_lossy();
            let media_type = guess_media_type(&path_str);
            let id = format!("asset_{}", i);
            let href = format!("OEBPS/{}", sanitize_path(&path_str));

            manifest_items.push(ManifestItem {
                id: id.clone(),
                href: href.clone(),
                media_type,
            });
            asset_map.insert(path_str.to_string(), href);
        }

        // EPUB 3 navigation document (required by spec; Apple Books rejects
        // EPUB 3 packages missing it). NCX remains for backwards-compat.
        manifest_items.push(ManifestItem {
            id: "nav".to_string(),
            href: "OEBPS/nav.xhtml".to_string(),
            media_type: "application/xhtml+xml".to_string(),
        });

        // 4. Write content.opf
        let cover_id = find_cover_manifest_id(book.metadata(), &manifest_items);
        let opf = generate_opf(
            book.metadata(),
            &manifest_items,
            &spine_refs,
            cover_id.as_deref(),
        );
        zip.start_file("OEBPS/content.opf", deflated)
            .map_err(io_error)?;
        zip.write_all(opf.as_bytes())?;

        // 5a. Write nav.xhtml (EPUB 3 navigation document)
        let nav = generate_nav(book.metadata(), book.toc(), book.landmarks());
        zip.start_file("OEBPS/nav.xhtml", deflated)
            .map_err(io_error)?;
        zip.write_all(nav.as_bytes())?;

        // 5b. Write toc.ncx (legacy fallback for EPUB 2 readers)
        let ncx = generate_ncx(book.metadata(), book.toc());
        zip.start_file("OEBPS/toc.ncx", deflated)
            .map_err(io_error)?;
        zip.write_all(ncx.as_bytes())?;

        // 6. Write chapters
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

        // 7. Write assets
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

        // 3. Build manifest
        let mut manifest_items: Vec<ManifestItem> = Vec::new();
        let mut spine_refs: Vec<String> = Vec::new();

        // Add stylesheet to manifest
        if !content.css.is_empty() {
            manifest_items.push(ManifestItem {
                id: "stylesheet".to_string(),
                href: "OEBPS/style.css".to_string(),
                media_type: "text/css".to_string(),
            });
        }

        // Add chapters to manifest
        for (i, _) in content.chapters.iter().enumerate() {
            let id = format!("chapter_{}", i);
            let href = format!("OEBPS/chapter_{}.xhtml", i);

            manifest_items.push(ManifestItem {
                id: id.clone(),
                href,
                media_type: "application/xhtml+xml".to_string(),
            });
            spine_refs.push(id);
        }

        // Add assets to manifest (from normalized content). Sort for stable
        // ordering across runs, and force-include the cover image even if no
        // chapter references it inline (Amazon's KFX stores the cover as a
        // bcRawMedia that's only mentioned in metadata).
        let mut asset_paths: Vec<String> = content.assets.iter().cloned().collect();
        asset_paths.sort();
        if let Some(cover) = book.metadata().cover_image.as_ref()
            && !cover.trim().is_empty()
            && !asset_paths.iter().any(|a| a == cover)
        {
            asset_paths.push(cover.clone());
        }
        // Pre-load asset bytes so we can sniff MIME types for extensionless
        // assets (KFX stores resources as `e0`/`eF`/… without extensions). We
        // reuse the bytes when writing to the zip below.
        let mut asset_bytes: Vec<(String, Vec<u8>)> = Vec::with_capacity(asset_paths.len());
        for asset_path in &asset_paths {
            let bytes = book
                .load_asset(std::path::Path::new(asset_path))
                .unwrap_or_default();
            asset_bytes.push((asset_path.clone(), bytes));
        }
        for (asset_idx, (asset_path, bytes)) in asset_bytes.iter().enumerate() {
            let mut media_type = guess_media_type(asset_path);
            if media_type == "application/octet-stream"
                && let Some(sniffed) = sniff_image_media_type(bytes)
            {
                media_type = sniffed.to_string();
            }
            let id = format!("asset_{}", asset_idx);
            let href = format!("OEBPS/{}", sanitize_path(asset_path));

            manifest_items.push(ManifestItem {
                id,
                href,
                media_type,
            });
        }

        // EPUB 3 navigation document (required by spec; Apple Books rejects
        // EPUB 3 packages missing it). NCX remains for backwards-compat.
        manifest_items.push(ManifestItem {
            id: "nav".to_string(),
            href: "OEBPS/nav.xhtml".to_string(),
            media_type: "application/xhtml+xml".to_string(),
        });

        // 4. Write content.opf
        let cover_id = find_cover_manifest_id(book.metadata(), &manifest_items);
        let opf = generate_opf(
            book.metadata(),
            &manifest_items,
            &spine_refs,
            cover_id.as_deref(),
        );
        zip.start_file("OEBPS/content.opf", deflated)
            .map_err(io_error)?;
        zip.write_all(opf.as_bytes())?;

        // 5a. Write nav.xhtml (EPUB 3 navigation document)
        let nav = generate_nav(book.metadata(), book.toc(), book.landmarks());
        zip.start_file("OEBPS/nav.xhtml", deflated)
            .map_err(io_error)?;
        zip.write_all(nav.as_bytes())?;

        // 5b. Write toc.ncx (legacy fallback for EPUB 2 readers)
        let ncx = generate_ncx(book.metadata(), book.toc());
        zip.start_file("OEBPS/toc.ncx", deflated)
            .map_err(io_error)?;
        zip.write_all(ncx.as_bytes())?;

        // 6. Write unified stylesheet
        if !content.css.is_empty() {
            zip.start_file("OEBPS/style.css", deflated)
                .map_err(io_error)?;
            zip.write_all(content.css.as_bytes())?;
        }

        // 7. Write synthesized chapters
        for (i, chapter) in content.chapters.iter().enumerate() {
            let zip_path = format!("OEBPS/chapter_{}.xhtml", i);
            zip.start_file(&zip_path, deflated).map_err(io_error)?;
            zip.write_all(chapter.document.as_bytes())?;
        }

        // 8. Write assets (reuse the bytes we already loaded for MIME sniffing).
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

struct ManifestItem {
    id: String,
    href: String,
    media_type: String,
}

/// Find the manifest item whose asset corresponds to `metadata.cover_image`.
///
/// `cover_image` carries the value populated by the importer: for an EPUB
/// source this is typically a path like `images/cover.jpg`; for a KFX source
/// it's a resource name like `eF`. Manifest hrefs always end with the asset's
/// raw path (e.g. `OEBPS/eF` or `OEBPS/images/cover.jpg`), so a suffix match
/// covers both cases without needing format-aware normalization.
fn find_cover_manifest_id(
    metadata: &crate::model::Metadata,
    manifest: &[ManifestItem],
) -> Option<String> {
    let cover = metadata.cover_image.as_ref()?;
    let cover_trim = cover.trim_start_matches('/');
    if cover_trim.is_empty() {
        return None;
    }
    manifest
        .iter()
        .find(|item| {
            let h = item.href.strip_prefix("OEBPS/").unwrap_or(&item.href);
            h == cover_trim || h.ends_with(cover_trim)
        })
        .map(|item| item.id.clone())
}

/// Generate content.opf from metadata and manifest.
///
/// `cover_manifest_id`: when set, emits `<meta name="cover" content="...">` in
/// the metadata block and `properties="cover-image"` on the matching manifest
/// item. EPUB2 readers find the cover via the meta; EPUB3 readers via the
/// properties attribute. We emit both for compatibility.
fn generate_opf(
    metadata: &crate::model::Metadata,
    manifest: &[ManifestItem],
    spine_refs: &[String],
    cover_manifest_id: Option<&str>,
) -> String {
    let mut opf = String::new();

    // Use EPUB3 for extended metadata support
    opf.push_str(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="BookId">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
"#,
    );

    // Track IDs for refinements
    let mut next_id = 1;

    // Title with optional file-as refinement
    let title_id = format!("title{}", next_id);
    next_id += 1;
    opf.push_str(&format!(
        "    <dc:title id=\"{}\">{}</dc:title>\n",
        title_id,
        escape_xml(&metadata.title)
    ));
    if let Some(ref title_sort) = metadata.title_sort {
        opf.push_str(&format!(
            "    <meta refines=\"#{}\" property=\"file-as\">{}</meta>\n",
            title_id,
            escape_xml(title_sort)
        ));
    }

    // Authors with optional file-as refinement for first author
    for (i, author) in metadata.authors.iter().enumerate() {
        let creator_id = format!("creator{}", next_id);
        next_id += 1;
        opf.push_str(&format!(
            "    <dc:creator id=\"{}\">{}</dc:creator>\n",
            creator_id,
            escape_xml(author)
        ));
        // Add file-as for first author if available
        if i == 0
            && let Some(ref author_sort) = metadata.author_sort
        {
            opf.push_str(&format!(
                "    <meta refines=\"#{}\" property=\"file-as\">{}</meta>\n",
                creator_id,
                escape_xml(author_sort)
            ));
        }
    }

    // Language
    if !metadata.language.is_empty() {
        opf.push_str(&format!(
            "    <dc:language>{}</dc:language>\n",
            escape_xml(&metadata.language)
        ));
    } else {
        opf.push_str("    <dc:language>en</dc:language>\n");
    }

    // Identifier
    if !metadata.identifier.is_empty() {
        opf.push_str(&format!(
            "    <dc:identifier id=\"BookId\">{}</dc:identifier>\n",
            escape_xml(&metadata.identifier)
        ));
    } else {
        opf.push_str("    <dc:identifier id=\"BookId\">urn:uuid:00000000-0000-0000-0000-000000000000</dc:identifier>\n");
    }

    // dcterms:modified (required for EPUB 3) — stamp conversion time, not the
    // source value. See [[feedback-modified-date-is-conversion-time]].
    opf.push_str(&format!(
        "    <meta property=\"dcterms:modified\">{}</meta>\n",
        escape_xml(&crate::util::time_now_iso8601_utc())
    ));

    // Contributors with role refinements
    for contrib in &metadata.contributors {
        let contrib_id = format!("contrib{}", next_id);
        next_id += 1;
        opf.push_str(&format!(
            "    <dc:contributor id=\"{}\">{}</dc:contributor>\n",
            contrib_id,
            escape_xml(&contrib.name)
        ));
        if let Some(ref role) = contrib.role {
            opf.push_str(&format!(
                "    <meta refines=\"#{}\" property=\"role\" scheme=\"marc:relators\">{}</meta>\n",
                contrib_id,
                escape_xml(role)
            ));
        }
        if let Some(ref file_as) = contrib.file_as {
            opf.push_str(&format!(
                "    <meta refines=\"#{}\" property=\"file-as\">{}</meta>\n",
                contrib_id,
                escape_xml(file_as)
            ));
        }
    }

    // Collection/series info
    if let Some(ref coll) = metadata.collection {
        let coll_id = format!("collection{}", next_id);
        next_id += 1;
        opf.push_str(&format!(
            "    <meta property=\"belongs-to-collection\" id=\"{}\">{}</meta>\n",
            coll_id,
            escape_xml(&coll.name)
        ));
        if let Some(ref coll_type) = coll.collection_type {
            opf.push_str(&format!(
                "    <meta refines=\"#{}\" property=\"collection-type\">{}</meta>\n",
                coll_id,
                escape_xml(coll_type)
            ));
        }
        if let Some(pos) = coll.position {
            let pos_str = if pos.fract() == 0.0 {
                format!("{}", pos as i64)
            } else {
                format!("{}", pos)
            };
            opf.push_str(&format!(
                "    <meta refines=\"#{}\" property=\"group-position\">{}</meta>\n",
                coll_id, pos_str
            ));
        }
    }

    // Suppress unused variable warning
    let _ = next_id;

    // Optional metadata
    if let Some(ref publisher) = metadata.publisher {
        opf.push_str(&format!(
            "    <dc:publisher>{}</dc:publisher>\n",
            escape_xml(publisher)
        ));
    }
    if let Some(ref description) = metadata.description {
        opf.push_str(&format!(
            "    <dc:description>{}</dc:description>\n",
            escape_xml(description)
        ));
    }
    for subject in &metadata.subjects {
        opf.push_str(&format!(
            "    <dc:subject>{}</dc:subject>\n",
            escape_xml(subject)
        ));
    }
    if let Some(ref date) = metadata.date {
        opf.push_str(&format!("    <dc:date>{}</dc:date>\n", escape_xml(date)));
    }
    if let Some(ref rights) = metadata.rights {
        opf.push_str(&format!(
            "    <dc:rights>{}</dc:rights>\n",
            escape_xml(rights)
        ));
    }

    // EPUB2-style cover declaration. EPUB3 readers also accept this and many
    // tools (Calibre, ADE) only find the cover via this meta.
    if let Some(cover_id) = cover_manifest_id {
        opf.push_str(&format!(
            "    <meta name=\"cover\" content=\"{}\"/>\n",
            escape_xml(cover_id)
        ));
    }

    opf.push_str("  </metadata>\n");

    // Manifest
    opf.push_str("  <manifest>\n");
    opf.push_str(
        "    <item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n",
    );

    for item in manifest {
        // Get relative path from OEBPS/
        let href = item.href.strip_prefix("OEBPS/").unwrap_or(&item.href);
        let properties = if item.id == "nav" {
            " properties=\"nav\""
        } else if Some(item.id.as_str()) == cover_manifest_id {
            " properties=\"cover-image\""
        } else {
            ""
        };
        opf.push_str(&format!(
            "    <item id=\"{}\" href=\"{}\" media-type=\"{}\"{}/>\n",
            escape_xml(&item.id),
            escape_xml(href),
            escape_xml(&item.media_type),
            properties
        ));
    }
    opf.push_str("  </manifest>\n");

    // Spine. Carry page-progression-direction through for vertical-RTL books
    // — without it, Kindle/Calibre paginate in the wrong direction.
    let spine_open = match metadata.page_progression_direction.as_deref() {
        Some(dir @ ("rtl" | "ltr" | "default")) => {
            format!("  <spine toc=\"ncx\" page-progression-direction=\"{}\">\n", dir)
        }
        _ => "  <spine toc=\"ncx\">\n".to_string(),
    };
    opf.push_str(&spine_open);
    for id in spine_refs {
        opf.push_str(&format!("    <itemref idref=\"{}\"/>\n", escape_xml(id)));
    }
    opf.push_str("  </spine>\n");

    opf.push_str("</package>\n");
    opf
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
        s.push_str("    <ol>\n      <li><a href=\"chapter_0.xhtml\">");
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

/// Sanitize a path for use in ZIP (remove leading slashes, normalize).
fn sanitize_path(path: &str) -> String {
    path.trim_start_matches('/')
        .replace('\\', "/")
        .replace("//", "/")
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
