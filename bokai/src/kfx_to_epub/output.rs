//! EPUB writer for the mechanical port.
//!
//! Mirrors calibre's `EPUB_Output` minimally: keeps an ordered manifest, an
//! ordered spine, a flat map of OEBPS files, and emits a valid EPUB3 zip on
//! `finalize`. The full calibre class is ~1.5K LOC and does a lot more
//! (NCX/nav3, beautify, dedupe, viewport, cover-detection); we add those
//! pieces as the port needs them.

use std::collections::HashMap;
use std::io::{Cursor, Write};

use zip::CompressionMethod;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use super::loader::BookMetadata;

/// One file we will ship inside `OEBPS/`. Order is preserved so the OPF
/// manifest reflects insertion order.
pub struct OebpsFile {
    pub data: Vec<u8>,
    pub mimetype: String,
    /// Optional pixel dimensions, used when we eventually emit per-image
    /// `<meta name="cover" content="..."/>` or fixed-layout viewports.
    #[allow(dead_code)]
    pub width: Option<u32>,
    #[allow(dead_code)]
    pub height: Option<u32>,
}

/// One spine entry: a manifest item id plus an optional EPUB `properties`
/// value. Fixed-layout pages carry `page-spread-left`/`page-spread-right` so
/// readers pair facing pages in a two-up view.
pub struct SpineItem {
    pub id: String,
    pub properties: Option<String>,
}

/// OPF manifest entry. `id` must be unique across the manifest; `href` is
/// relative to the OPF (i.e. the file path under `OEBPS/`).
pub struct ManifestEntry {
    pub id: String,
    pub href: String,
    pub media_type: String,
    /// Cover image marker. EPUB2 expects a `<meta name="cover" content="id"/>`
    /// in `<metadata>`; EPUB3 uses `properties="cover-image"` on the manifest
    /// item. We emit both for max reader compatibility.
    pub is_cover_image: bool,
    /// Nav doc marker (EPUB3).
    #[allow(dead_code)]
    pub is_nav: bool,
}

/// EPUB output state shared across the conversion.
pub struct EpubOutput {
    /// File path under `OEBPS/` → bytes. Insertion order tracked separately.
    oebps_files: HashMap<String, OebpsFile>,
    /// Insertion order of `oebps_files` keys.
    oebps_order: Vec<String>,

    /// Manifest entries, insertion order.
    manifest: Vec<ManifestEntry>,
    /// `id` → index in `manifest` for fast lookup / linking.
    manifest_by_id: HashMap<String, usize>,

    /// Spine: ordered list of manifest item ids (+ optional FXL properties).
    spine: Vec<SpineItem>,

    /// Image-based fixed-layout book (manga / comic). Drives the
    /// `rendition:layout pre-paginated` + `fixed-layout` OPF metadata and the
    /// `rendition:` property declaration on `<package>`.
    pub fixed_layout: bool,
    /// Page pixel size for `original-resolution` / `rendition:orientation`
    /// (calibre `epub_output.py:932`). Only meaningful when `fixed_layout`.
    pub original_resolution: Option<(u32, u32)>,
    /// `book-type` OPF hint — `"comic"` for double-page-spread manga
    /// (calibre `epub_output.py:941`).
    pub book_type: Option<String>,

    /// TOC tree (reading-order sorted), rendered into both the EPUB 3 nav
    /// doc and the NCX navMap by the shared `export::nav` emitters. Empty
    /// falls back to a single entry pointing at the first spine chapter.
    pub toc: Vec<crate::export::nav::NavPoint>,

    /// Physical page list (flat, page order), rendered as
    /// `<nav epub:type="page-list">`. Populated in `mod.rs` from
    /// `navigation::extract_page_list` when the source KFX carries a
    /// `page_list` nav_container; empty omits the nav.
    pub page_list: Vec<crate::export::nav::NavPoint>,

    /// Page-progression-direction. Mirrors calibre's `EPUB_Output.
    /// page_progression_direction`. Emitted to `<spine
    /// page-progression-direction="...">` only when set and not `"ltr"`
    /// (calibre suppresses the attribute for the EPUB default — `ltr`).
    pub page_progression_direction: Option<String>,

    /// Book-level writing mode (e.g. `vertical-rl`). When set and not
    /// `horizontal-tb`, the OPF emits `<meta name="primary-writing-mode">`
    /// as a Kindle reader hint — mirrors calibre's `epub_output.py:955+`.
    pub writing_mode: Option<String>,

    /// OPF `<guide>` entries (EPUB 2.0 landmark references). Populated from
    /// KFX `nav_type=landmarks` containers; emitted as
    /// `<reference type="..." href="..." title="..."/>` inside `<guide>`
    /// and re-mapped to the EPUB 3 vocabulary for the nav doc's landmarks.
    pub guide: Vec<crate::export::opf::OpfGuideRef>,
}

impl EpubOutput {
    pub fn new() -> Self {
        Self {
            oebps_files: HashMap::new(),
            oebps_order: Vec::new(),
            manifest: Vec::new(),
            manifest_by_id: HashMap::new(),
            spine: Vec::new(),
            fixed_layout: false,
            original_resolution: None,
            book_type: None,
            toc: Vec::new(),
            page_list: Vec::new(),
            page_progression_direction: None,
            writing_mode: None,
            guide: Vec::new(),
        }
    }

    /// Reserve a manifest id derived from `filename` (no path, no extension).
    /// EPUB ids must start with a letter; we prefix with `id_` when needed.
    pub fn make_id(&self, filename: &str) -> String {
        crate::export::opf::make_manifest_id(filename, |id| self.manifest_by_id.contains_key(id))
    }

    /// Add a file under `OEBPS/` and register a manifest entry. Returns the
    /// manifest id assigned (also stored as `manifest.last().id`).
    pub fn add_resource(
        &mut self,
        filename: &str,
        data: Vec<u8>,
        mimetype: &str,
        width: Option<u32>,
        height: Option<u32>,
    ) -> String {
        let id = self.make_id(filename);
        self.oebps_files.insert(
            filename.to_string(),
            OebpsFile {
                data,
                mimetype: mimetype.to_string(),
                width,
                height,
            },
        );
        self.oebps_order.push(filename.to_string());
        let idx = self.manifest.len();
        self.manifest.push(ManifestEntry {
            id: id.clone(),
            href: filename.to_string(),
            media_type: mimetype.to_string(),
            is_cover_image: false,
            is_nav: false,
        });
        self.manifest_by_id.insert(id.clone(), idx);
        id
    }

    /// Fill in the bytes of a resource registered with empty data (the
    /// deferred-image pass). `mime` overrides the predicted media type when
    /// the transcode's actual output differs (a broken JXR passing through as
    /// `image/jxr` instead of the predicted `image/jpeg`). Returns false when
    /// no such file exists (e.g. it was pruned).
    pub fn fill_resource_bytes(
        &mut self,
        filename: &str,
        data: Vec<u8>,
        mime: Option<&str>,
    ) -> bool {
        let Some(file) = self.oebps_files.get_mut(filename) else {
            return false;
        };
        file.data = data;
        if let Some(m) = mime
            && file.mimetype != m
        {
            file.mimetype = m.to_string();
            if let Some(entry) = self.manifest.iter_mut().find(|e| e.href == filename) {
                entry.media_type = m.to_string();
            }
        }
        true
    }

    /// Mark the manifest entry with the given id as the cover image.
    pub fn mark_cover(&mut self, manifest_id: &str) {
        if let Some(&idx) = self.manifest_by_id.get(manifest_id) {
            self.manifest[idx].is_cover_image = true;
        }
    }

    /// Rename a bundled resource: change both its filename (in
    /// `oebps_files` / `oebps_order`) and its manifest entry (href + id).
    /// Returns the new manifest id (callers may need it for cross-references
    /// like spine `idref` or `<meta name="cover" content="...">`).
    pub fn rename_resource(
        &mut self,
        old_filename: &str,
        new_filename: &str,
        new_id: Option<&str>,
    ) -> Option<String> {
        // Move the file blob.
        let blob = self.oebps_files.remove(old_filename)?;
        self.oebps_files.insert(new_filename.to_string(), blob);
        for slot in &mut self.oebps_order {
            if slot == old_filename {
                *slot = new_filename.to_string();
            }
        }
        // Update the manifest entry. The old id is keyed by old filename;
        // a caller-supplied `new_id` overrides the auto-derived id.
        let old_id = self
            .manifest
            .iter()
            .find(|m| m.href == old_filename)
            .map(|m| m.id.clone())?;
        let new_id_str = new_id
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.make_id(new_filename));
        let idx = *self.manifest_by_id.get(&old_id)?;
        self.manifest[idx].href = new_filename.to_string();
        self.manifest[idx].id = new_id_str.clone();
        // Re-index the lookup map.
        self.manifest_by_id.remove(&old_id);
        self.manifest_by_id.insert(new_id_str.clone(), idx);
        // Spine entries are by id — update any reference to the old id.
        for slot in &mut self.spine {
            if slot.id == old_id {
                slot.id = new_id_str.clone();
            }
        }
        Some(new_id_str)
    }

    /// Insert a chapter at the beginning of the spine (rather than appended).
    /// Used for cover wrappers like `titlepage.xhtml` that must render before
    /// the first KFX section.
    pub fn prepend_spine_chapter(&mut self, filename: &str, xhtml: String) {
        let id = self.add_resource(
            filename,
            xhtml.into_bytes(),
            "application/xhtml+xml",
            None,
            None,
        );
        self.spine.insert(
            0,
            SpineItem {
                id,
                properties: None,
            },
        );
    }

    /// Public read-only accessor used by `mod.rs` when generating the
    /// titlepage (we need the cover image filename + dimensions to size the
    /// SVG viewBox).
    pub fn cover_image_info(&self) -> Option<(&str, Option<u32>, Option<u32>)> {
        let entry = self.manifest.iter().find(|m| m.is_cover_image)?;
        let file = self.oebps_files.get(&entry.href)?;
        Some((entry.href.as_str(), file.width, file.height))
    }

    /// Whether a file already exists under `OEBPS/<filename>`.
    pub fn has_file(&self, filename: &str) -> bool {
        self.oebps_files.contains_key(filename)
    }

    /// Read-only view of the manifest so adjacent steps can iterate without
    /// taking a mutable borrow (e.g. scaffold-chapter emission).
    pub fn manifest_view(&self) -> &[ManifestEntry] {
        &self.manifest
    }

    /// Map each spine item's file (`href`, e.g. `c18.xhtml`) to its 0-based
    /// position in the spine — i.e. its reading-order rank. Used to sort the
    /// TOC into reading order (EPUB 3 nav requirement; epubcheck NAV-011).
    pub fn spine_file_rank(&self) -> HashMap<String, usize> {
        let mut rank = HashMap::new();
        for (i, item) in self.spine.iter().enumerate() {
            if let Some(&idx) = self.manifest_by_id.get(&item.id) {
                rank.entry(self.manifest[idx].href.clone()).or_insert(i);
            }
        }
        rank
    }

    /// Append a chapter `xhtml` to the spine, bundled at `OEBPS/<filename>`.
    pub fn add_spine_chapter(&mut self, filename: &str, xhtml: String) {
        self.add_spine_chapter_with_props(filename, xhtml, None);
    }

    /// Like [`add_spine_chapter`] but attaches an EPUB itemref `properties`
    /// value (e.g. `page-spread-left`) for fixed-layout pages.
    pub fn add_spine_chapter_with_props(
        &mut self,
        filename: &str,
        xhtml: String,
        properties: Option<String>,
    ) {
        let id = self.add_resource(
            filename,
            xhtml.into_bytes(),
            "application/xhtml+xml",
            None,
            None,
        );
        self.spine.push(SpineItem { id, properties });
    }

    /// Drop manifest image resources not referenced by any spine document.
    /// Fixed-layout manga ships a full set of page thumbnails the reading order
    /// never uses (`yj_thumbnails_present`); calibre manifests only referenced
    /// resources. `referenced` is the set of `<img src>` hrefs collected from
    /// the emitted pages; the cover image is always kept. Returns the number of
    /// resources pruned.
    pub fn retain_referenced_images(
        &mut self,
        referenced: &std::collections::HashSet<String>,
    ) -> usize {
        let drop: Vec<String> = self
            .manifest
            .iter()
            .filter(|m| {
                m.media_type.starts_with("image/")
                    && !m.is_cover_image
                    && !referenced.contains(&m.href)
            })
            .map(|m| m.href.clone())
            .collect();
        for href in &drop {
            self.oebps_files.remove(href);
            self.oebps_order.retain(|f| f != href);
            if let Some(pos) = self.manifest.iter().position(|m| &m.href == href) {
                let id = self.manifest[pos].id.clone();
                self.manifest.remove(pos);
                self.manifest_by_id.remove(&id);
            }
        }
        // `manifest_by_id` indices shifted by the removals above — rebuild.
        if !drop.is_empty() {
            self.manifest_by_id.clear();
            for (i, m) in self.manifest.iter().enumerate() {
                self.manifest_by_id.insert(m.id.clone(), i);
            }
        }
        drop.len()
    }

    /// Reader-mode extraction: the spine chapters in reading order as
    /// `(href, xhtml)`. The Sidle reader renders these directly instead of
    /// zipping them into an EPUB. Chapter data is always UTF-8 (we add it via
    /// `xhtml.into_bytes()`), so the conversion is lossless.
    pub fn spine_documents(&self) -> Vec<(String, String)> {
        self.spine
            .iter()
            .filter_map(|item| {
                let idx = *self.manifest_by_id.get(&item.id)?;
                let href = self.manifest[idx].href.clone();
                let file = self.oebps_files.get(&href)?;
                Some((href, String::from_utf8_lossy(&file.data).into_owned()))
            })
            .collect()
    }

    /// Reader-mode extraction with fixed-layout metadata: `(href, html,
    /// spread_property)` per spine document in reading order. `spread_property`
    /// is `page-spread-left`/`-right` for paired FXL pages, else `None`. The
    /// per-page viewport lives in the document's own `<meta name="viewport">`.
    pub fn spine_documents_with_props(&self) -> Vec<(String, String, Option<String>)> {
        self.spine
            .iter()
            .filter_map(|item| {
                let idx = *self.manifest_by_id.get(&item.id)?;
                let href = self.manifest[idx].href.clone();
                let file = self.oebps_files.get(&href)?;
                Some((
                    href,
                    String::from_utf8_lossy(&file.data).into_owned(),
                    item.properties.clone(),
                ))
            })
            .collect()
    }

    /// Whether this is an image-based fixed-layout book (manga / comic).
    pub fn is_fixed_layout(&self) -> bool {
        self.fixed_layout
    }

    /// Reader-mode extraction: every non-spine file (images, `style.css`) as
    /// `(href, mimetype, bytes)`, in insertion order. The reader serves these
    /// to the render iframe (the chapters reference them by relative href).
    pub fn non_spine_resources(&self) -> Vec<(String, String, Vec<u8>)> {
        let spine_hrefs: std::collections::HashSet<&str> = self
            .spine
            .iter()
            .filter_map(|item| {
                self.manifest_by_id
                    .get(&item.id)
                    .map(|&i| self.manifest[i].href.as_str())
            })
            .collect();
        self.oebps_order
            .iter()
            .filter(|href| !spine_hrefs.contains(href.as_str()))
            .filter_map(|href| {
                let file = self.oebps_files.get(href)?;
                Some((href.clone(), file.mimetype.clone(), file.data.clone()))
            })
            .collect()
    }

    /// Finalize: build the EPUB zip in memory.
    pub fn finalize(&self, meta: &BookMetadata) -> std::io::Result<Vec<u8>> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut buf);
            let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            let deflated = SimpleFileOptions::default()
                .compression_method(CompressionMethod::Deflated)
                .compression_level(Some(6));

            // 1. mimetype (must be first, uncompressed)
            zip.start_file("mimetype", stored).map_err(io_error)?;
            zip.write_all(b"application/epub+zip")?;

            // 2. container.xml
            zip.start_file("META-INF/container.xml", deflated)
                .map_err(io_error)?;
            zip.write_all(CONTAINER_XML)?;

            // 3. content.opf
            let opf = self.generate_opf(meta);
            zip.start_file("OEBPS/content.opf", deflated)
                .map_err(io_error)?;
            zip.write_all(opf.as_bytes())?;

            // 4a. nav.xhtml (EPUB 3 navigation document — required by spec,
            //     enforced by Apple Books).
            let nav = self.generate_nav(meta);
            zip.start_file("OEBPS/nav.xhtml", deflated)
                .map_err(io_error)?;
            zip.write_all(nav.as_bytes())?;

            // 4b. toc.ncx (legacy fallback for EPUB 2 readers; kept alongside
            //     the nav doc).
            let ncx = self.generate_ncx(meta);
            zip.start_file("OEBPS/toc.ncx", deflated)
                .map_err(io_error)?;
            zip.write_all(ncx.as_bytes())?;

            // 5. OEBPS files in insertion order. Already-compressed media
            // types (JPEG / PNG / WEBP / GIF) get `Stored` — running deflate
            // over them produces <5% gain at ~10-15 ms per MB. On
            // image-heavy books that's ~80% of `finalize`'s cost wasted.
            // Text, CSS, OPF, NCX, XHTML still go through deflate (typical
            // 5-10× shrink). Both methods are valid EPUB; calibre defaults
            // to deflate everywhere and pays the same wasted cost.
            for filename in &self.oebps_order {
                let file = &self.oebps_files[filename];
                let zip_path = format!("OEBPS/{}", filename);
                let opts = if is_precompressed_mime(&file.mimetype) {
                    stored
                } else {
                    deflated
                };
                zip.start_file(&zip_path, opts).map_err(io_error)?;
                zip.write_all(&file.data)?;
            }

            zip.finish().map_err(io_error)?;
        }
        Ok(buf.into_inner())
    }

    /// Assemble the OPF package and serialize it through the shared emitter
    /// (`export::opf`) — the same code path the IR exporter uses, so both
    /// engines produce an identical package document from identical inputs.
    fn generate_opf(&self, meta: &BookMetadata) -> String {
        use crate::export::opf;

        // Authors — positional per-creator `author_pronunciation` sort keys
        // (yomigana), via the shared helper so both engines emit one shape.
        let file_as_keys = opf::creator_file_as_keys(&meta.authors, &meta.author_pronunciations);
        let creators = meta
            .authors
            .iter()
            .zip(&file_as_keys)
            .map(|(author, file_as)| opf::OpfCreator {
                name: author.clone(),
                role: Some("aut".to_string()),
                file_as: Some(file_as.clone()),
            })
            .collect();

        let manifest = self
            .manifest
            .iter()
            .map(|m| {
                let mut props: Vec<String> = Vec::new();
                if m.is_cover_image {
                    props.push("cover-image".to_string());
                }
                if m.is_nav {
                    props.push("nav".to_string());
                }
                // EPUB 3 (OPF-014): a content doc embedding inline SVG /
                // MathML / scripting must declare it in `properties`.
                if m.media_type == "application/xhtml+xml"
                    && let Some(f) = self.oebps_files.get(&m.href)
                {
                    let xml = String::from_utf8_lossy(&f.data);
                    props.extend(
                        opf::xhtml_content_properties(&xml)
                            .into_iter()
                            .map(str::to_string),
                    );
                }
                opf::OpfItem {
                    id: m.id.clone(),
                    href: m.href.clone(),
                    media_type: m.media_type.clone(),
                    properties: props,
                }
            })
            .collect();

        let spine = self
            .spine
            .iter()
            .map(|item| opf::OpfItemref {
                idref: item.id.clone(),
                properties: item.properties.clone(),
            })
            .collect();

        let guide = self
            .guide
            .iter()
            .map(|g| opf::OpfGuideRef {
                guide_type: g.guide_type.clone(),
                title: g.title.clone(),
                href: g.href.clone(),
            })
            .collect();

        let fixed_layout = self.fixed_layout.then(|| opf::OpfFixedLayout {
            rendition_spread: None,
            ebpaj_viewport: None,
            original_resolution: self.original_resolution,
            book_type: self.book_type.clone(),
        });

        let pkg = opf::OpfPackage {
            metadata: opf::OpfMetadata {
                title: meta.title.clone(),
                title_file_as: meta.title_pronunciation.clone(),
                creators,
                contributors: Vec::new(),
                language: meta.language.clone(),
                identifier: meta.identifier.clone(),
                asin: meta.asin.clone(),
                // Stamps the conversion time — never the source's value (the
                // modified date describes this file).
                modified: crate::util::time_now_iso8601_utc(),
                date: meta.issue_date.as_deref().map(opf::format_opf_date),
                publisher: meta.publisher.clone(),
                description: None,
                subjects: Vec::new(),
                rights: None,
                collection: None,
                cover_manifest_id: self
                    .manifest
                    .iter()
                    .find(|m| m.is_cover_image)
                    .map(|m| m.id.clone()),
                primary_writing_mode: opf::primary_writing_mode(
                    self.writing_mode.as_deref(),
                    self.page_progression_direction.as_deref(),
                ),
                page_progression_direction: self.page_progression_direction.clone(),
                fixed_layout,
            },
            manifest,
            spine,
            guide,
        };
        opf::emit_opf(&pkg)
    }

    /// The empty-TOC fallback target: the first spine document's href
    /// (`None` when the spine is empty). Shared by the nav doc and the NCX.
    fn toc_fallback_href(&self) -> Option<String> {
        let first = self.spine.first()?;
        Some(
            self.manifest_by_id
                .get(&first.id)
                .map(|&idx| self.manifest[idx].href.clone())
                .unwrap_or_default(),
        )
    }

    /// Assemble `nav.xhtml` through the shared emitter (`export::nav`) —
    /// the same code path the IR exporter uses.
    fn generate_nav(&self, meta: &BookMetadata) -> String {
        let fallback = self.toc_fallback_href();
        crate::export::nav::emit_nav(&crate::export::nav::NavDoc {
            title: &meta.title,
            language: &meta.language,
            toc: &self.toc,
            toc_fallback_href: fallback.as_deref(),
            page_list: &self.page_list,
            landmarks: &self.guide,
        })
    }

    /// Assemble `toc.ncx` through the shared emitter (`export::nav`).
    fn generate_ncx(&self, meta: &BookMetadata) -> String {
        let fallback = self.toc_fallback_href();
        crate::export::nav::emit_ncx(&crate::export::nav::NcxDoc {
            title: &meta.title,
            identifier: &meta.identifier,
            toc: &self.toc,
            toc_fallback_href: fallback.as_deref(),
        })
    }
}

impl Default for EpubOutput {
    fn default() -> Self {
        Self::new()
    }
}

const CONTAINER_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"#;

fn io_error<E: std::error::Error + Send + Sync + 'static>(e: E) -> std::io::Error {
    std::io::Error::other(e)
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

// `uuid_v5` lives in `crate::util` — shared with the generic exporter and
// the Aozora builder.
