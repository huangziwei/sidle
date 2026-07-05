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

    /// NCX navMap content (the inner <navPoint>…</navPoint> sequence).
    /// When set, `generate_ncx` uses this instead of the spine-derived
    /// fallback.
    pub ncx_navmap: Option<String>,

    /// EPUB 3 nav doc TOC `<ol>` body, paired with `ncx_navmap` (both
    /// derived from the same `NavPoint` tree). Populated alongside
    /// `ncx_navmap` in `mod.rs`. When set, `generate_nav` emits this
    /// inside `<nav epub:type="toc">`; otherwise falls back to a
    /// single-entry list pointing at the first spine chapter.
    pub nav_ol_html: Option<String>,

    /// EPUB 3 nav doc page-list `<ol>` body (flat), emitted inside
    /// `<nav epub:type="page-list">`. Populated in `mod.rs` from
    /// `navigation::extract_page_list` when the source KFX carries a
    /// `page_list` nav_container; `None` (nav omitted) otherwise.
    pub page_list_ol_html: Option<String>,

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
    /// `<reference type="..." href="..." title="..."/>` inside `<guide>`.
    pub guide: Vec<super::navigation::GuideRef>,
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
            ncx_navmap: None,
            nav_ol_html: None,
            page_list_ol_html: None,
            page_progression_direction: None,
            writing_mode: None,
            guide: Vec::new(),
        }
    }

    /// Reserve a manifest id derived from `filename` (no path, no extension).
    /// EPUB ids must start with a letter; we prefix with `id_` when needed.
    pub fn make_id(&self, filename: &str) -> String {
        let stem = filename
            .rsplit('/')
            .next()
            .unwrap_or(filename)
            .split('.')
            .next()
            .unwrap_or(filename);
        let mut id: String = stem
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if id.is_empty() || !id.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            id = format!("id_{}", id);
        }
        // Disambiguate against existing ids.
        if !self.manifest_by_id.contains_key(&id) {
            return id;
        }
        let mut n = 1;
        loop {
            let candidate = format!("{}_{}", id, n);
            if !self.manifest_by_id.contains_key(&candidate) {
                return candidate;
            }
            n += 1;
        }
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

    fn generate_opf(&self, meta: &BookMetadata) -> String {
        // EPUB 3.0 package. Earlier we downgraded to 2.0 to placate strict
        // readers (notably Apple Books) that rejected EPUB 3 packages
        // missing the spec-required `nav.xhtml` document. We now emit a
        // proper nav doc (see `generate_nav` + the `OEBPS/nav.xhtml`
        // entry written in `into_zip_bytes`), so EPUB 3 conformance is
        // back — with `properties="nav"` / `properties="cover-image"` on
        // manifest items and `<meta property="dcterms:modified">` in
        // metadata. NCX still emitted for legacy EPUB 2 readers.
        let mut s = String::new();
        // Declare the `rendition:` property vocabulary on `<package>` for
        // fixed-layout books (EPUB 3 Multiple-Rendition / FXL metadata).
        let prefix_attr = if self.fixed_layout {
            " prefix=\"rendition: http://www.idpf.org/vocab/rendition/#\""
        } else {
            ""
        };
        s.push_str(&format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"BookId\"{prefix_attr}>\n  <metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:opf=\"http://www.idpf.org/2007/opf\">\n"
        ));

        // Title — when KFX carries `title_pronunciation` (Japanese yomigana
        // sort key), surface it as `opf:file-as`; otherwise omit the attr.
        let title = if meta.title.is_empty() {
            "Untitled"
        } else {
            &meta.title
        };
        // Title — the yomigana sort key rides an EPUB-3 `<meta refines>`
        // (`opf:file-as` as a `<dc:title>` attribute is EPUB-2 only, rejected
        // by epubcheck as RSC-005 under 3.x).
        s.push_str(&format!(
            "    <dc:title id=\"title\">{}</dc:title>\n",
            xml_escape(title)
        ));
        if let Some(file_as) = meta.title_pronunciation.as_deref() {
            s.push_str(&format!(
                "    <meta refines=\"#title\" property=\"file-as\">{}</meta>\n",
                xml_escape(file_as)
            ));
        }

        // Authors — role + file-as via EPUB-3 `<meta refines>` (was EPUB-2
        // `opf:role`/`opf:file-as` attributes). Prefer the KFX-supplied
        // `author_pronunciation` (yomigana sort key); fall back to the joined
        // author list so EPUB libraries still sort multi-author books.
        let author_file_as = meta
            .author_pronunciation
            .clone()
            .unwrap_or_else(|| meta.authors.join(" & "));
        for (i, author) in meta.authors.iter().enumerate() {
            let cid = format!("creator{}", i + 1);
            s.push_str(&format!(
                "    <dc:creator id=\"{}\">{}</dc:creator>\n",
                cid,
                xml_escape(author)
            ));
            s.push_str(&format!(
                "    <meta refines=\"#{}\" property=\"role\" scheme=\"marc:relators\">aut</meta>\n",
                cid
            ));
            s.push_str(&format!(
                "    <meta refines=\"#{}\" property=\"file-as\">{}</meta>\n",
                cid,
                xml_escape(&author_file_as)
            ));
        }

        // Language
        let lang = if meta.language.is_empty() {
            "en"
        } else {
            &meta.language
        };
        s.push_str(&format!(
            "    <dc:language>{}</dc:language>\n",
            xml_escape(lang)
        ));

        // Identifier — the primary unique-id is the KFX book_id. EPUB 3.x
        // forbids the EPUB-2 `opf:scheme` attribute on `<dc:identifier>`
        // (RSC-005), so the ASIN rides a plain identifier tagged `id="asin"`;
        // `import::epub` recovers it from that id (round-trips ASIN back to
        // KFX). The MOBI-ASIN / uuid scheme twins were calibre-isms with no
        // consumer and are dropped.
        let id = if meta.identifier.is_empty() {
            "urn:uuid:00000000-0000-0000-0000-000000000000"
        } else {
            &meta.identifier
        };
        s.push_str(&format!(
            "    <dc:identifier id=\"BookId\">{}</dc:identifier>\n",
            xml_escape(id)
        ));
        if let Some(asin) = meta.asin.as_deref() {
            s.push_str(&format!(
                "    <dc:identifier id=\"asin\">{}</dc:identifier>\n",
                xml_escape(asin)
            ));
        }

        // `dcterms:modified` — required by EPUB 3 (every Publication must
        // declare its last-modified time). Stamps NOW per
        // [[feedback_modified_date_is_conversion_time]] — never the
        // source's value.
        s.push_str(&format!(
            "    <meta property=\"dcterms:modified\">{}</meta>\n",
            xml_escape(&crate::util::time_now_iso8601_utc())
        ));

        // Publication date — calibre uses `kindle_title_metadata/issue_date`
        // (KFX stores as YYYY-MM-DD). Emit as ISO-8601 with a UTC offset to
        // match calibre's output format.
        if let Some(date) = meta.issue_date.as_deref() {
            let iso = if date.len() == 10
                && date.chars().nth(4) == Some('-')
                && date.chars().nth(7) == Some('-')
            {
                format!("{}T00:00:00+00:00", date)
            } else {
                date.to_string()
            };
            s.push_str(&format!("    <dc:date>{}</dc:date>\n", xml_escape(&iso)));
        }

        // Publisher (optional). Skip when empty/whitespace: the OPF schema
        // requires `<dc:publisher>` to carry a non-empty string, so an empty
        // element is RSC-005 ("character content … invalid").
        if let Some(pub_) = meta
            .publisher
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            s.push_str(&format!(
                "    <dc:publisher>{}</dc:publisher>\n",
                xml_escape(pub_)
            ));
        }

        // Cover meta (EPUB2-compat)
        if let Some(cover_id) = self
            .manifest
            .iter()
            .find(|m| m.is_cover_image)
            .map(|m| &m.id)
        {
            s.push_str(&format!(
                "    <meta name=\"cover\" content=\"{}\"/>\n",
                xml_escape(cover_id)
            ));
        }

        // Primary writing mode hint (calibre epub_output.py:954). For a
        // horizontal book the primary mode encodes the page-turn direction
        // (`horizontal-rl` for RTL manga); for a vertical book it's the
        // writing mode itself. `horizontal-lr` is the default and omitted.
        let wm = self.writing_mode.as_deref().unwrap_or("horizontal-tb");
        let ppd = self.page_progression_direction.as_deref().unwrap_or("ltr");
        let primary_writing_mode = if wm == "horizontal-tb" || wm.is_empty() {
            if ppd == "rtl" {
                "horizontal-rl"
            } else {
                "horizontal-lr"
            }
        } else {
            wm
        };
        if primary_writing_mode != "horizontal-lr" {
            s.push_str(&format!(
                "    <meta name=\"primary-writing-mode\" content=\"{}\"/>\n",
                xml_escape(primary_writing_mode)
            ));
        }

        // Fixed-layout (manga / comic) metadata — calibre epub_output.py:926.
        if self.fixed_layout {
            s.push_str("    <meta property=\"rendition:layout\">pre-paginated</meta>\n");
            s.push_str("    <meta name=\"fixed-layout\" content=\"true\"/>\n");
            if let Some((w, h)) = self.original_resolution {
                s.push_str(&format!(
                    "    <meta name=\"original-resolution\" content=\"{w}x{h}\"/>\n"
                ));
                let orientation = if w > h { "landscape" } else { "portrait" };
                s.push_str(&format!(
                    "    <meta property=\"rendition:orientation\">{orientation}</meta>\n"
                ));
                s.push_str(&format!(
                    "    <meta name=\"orientation-lock\" content=\"{orientation}\"/>\n"
                ));
            }
            if let Some(bt) = self.book_type.as_deref() {
                s.push_str(&format!(
                    "    <meta name=\"book-type\" content=\"{}\"/>\n",
                    xml_escape(bt)
                ));
            }
        }

        s.push_str("  </metadata>\n");

        // Manifest. The `<meta name="cover">` marker in metadata is kept
        // alongside `properties="cover-image"` here for EPUB-2-reader
        // compatibility — both are honoured by most readers, and emitting
        // both is the calibre convention.
        s.push_str("  <manifest>\n");
        s.push_str(
            "    <item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n",
        );
        s.push_str(
            "    <item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n",
        );
        for m in &self.manifest {
            let mut props: Vec<&str> = Vec::new();
            if m.is_cover_image {
                props.push("cover-image");
            }
            if m.is_nav {
                props.push("nav");
            }
            // EPUB 3 (OPF-014): a content doc embedding inline SVG / MathML /
            // scripting must declare it in the manifest `properties`. Scan the
            // XHTML bytes for real element openings — text-node `<` is escaped
            // as `&lt;` in XHTML, so a raw `<svg` is always a genuine element,
            // and we must not over-declare (the inverse, OPF-015).
            if m.media_type == "application/xhtml+xml"
                && let Some(f) = self.oebps_files.get(&m.href)
            {
                let xml = String::from_utf8_lossy(&f.data);
                if contains_element(&xml, "svg") {
                    props.push("svg");
                }
                if contains_element(&xml, "math") {
                    props.push("mathml");
                }
                if contains_element(&xml, "script") {
                    props.push("scripted");
                }
            }
            let properties = if props.is_empty() {
                String::new()
            } else {
                format!(" properties=\"{}\"", props.join(" "))
            };
            s.push_str(&format!(
                "    <item id=\"{}\" href=\"{}\" media-type=\"{}\"{}/>\n",
                xml_escape(&m.id),
                xml_escape(&m.href),
                xml_escape(&m.media_type),
                properties,
            ));
        }
        s.push_str("  </manifest>\n");

        // Spine — calibre emits page-progression-direction only when it
        // diverges from the default `ltr` (epub_output.py:1052).
        let ppd_attr = self
            .page_progression_direction
            .as_deref()
            .filter(|v| !v.is_empty() && *v != "ltr")
            .map(|v| format!(" page-progression-direction=\"{}\"", xml_escape(v)))
            .unwrap_or_default();
        s.push_str(&format!("  <spine toc=\"ncx\"{}>\n", ppd_attr));
        for item in &self.spine {
            let props = item
                .properties
                .as_deref()
                .map(|p| format!(" properties=\"{}\"", xml_escape(p)))
                .unwrap_or_default();
            s.push_str(&format!(
                "    <itemref idref=\"{}\"{}/>\n",
                xml_escape(&item.id),
                props,
            ));
        }
        s.push_str("  </spine>\n");

        // `<guide>` (EPUB 2.0 landmarks). Mirrors calibre's
        // `add_guide_entry` output. Each entry is one
        // `<reference type="..." title="..." href="..."/>`. Skipped when
        // empty so the OPF stays clean for inputs with no landmark
        // metadata.
        if !self.guide.is_empty() {
            s.push_str("  <guide>\n");
            for g in &self.guide {
                s.push_str(&format!(
                    "    <reference type=\"{}\" title=\"{}\" href=\"{}\"/>\n",
                    xml_escape(&g.guide_type),
                    xml_escape(&g.label),
                    xml_escape(&g.href),
                ));
            }
            s.push_str("  </guide>\n");
        }

        s.push_str("</package>\n");
        s
    }

    /// Generate `nav.xhtml`, the EPUB 3 navigation document.
    ///
    /// The W3C EPUB 3.3 spec requires every Publication to include exactly
    /// one nav doc, and conformant readers (Apple Books) reject EPUB 3
    /// packages without it. NCX no longer satisfies the requirement on
    /// its own — it's strictly legacy.
    ///
    /// Body shape (mirrors calibre's EPUB 3 nav output):
    /// `<nav epub:type="toc"><ol><li><a href=…>…</a></li></ol></nav>`,
    /// optional `<nav epub:type="landmarks">` from `self.guide` (the same
    /// container used to emit the EPUB-2 `<guide>` block).
    fn generate_nav(&self, meta: &BookMetadata) -> String {
        let title = if meta.title.is_empty() {
            "Untitled"
        } else {
            &meta.title
        };
        let lang = if meta.language.is_empty() {
            "en"
        } else {
            meta.language.as_str()
        };

        let mut s = String::new();
        s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        s.push_str("<!DOCTYPE html>\n");
        s.push_str(&format!(
            "<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\" xml:lang=\"{lang}\" lang=\"{lang}\">\n",
            lang = xml_escape(lang),
        ));
        s.push_str("<head>\n");
        s.push_str("  <meta charset=\"utf-8\"/>\n");
        s.push_str(&format!("  <title>{}</title>\n", xml_escape(title)));
        s.push_str("</head>\n<body>\n");

        // TOC nav.
        s.push_str("  <nav epub:type=\"toc\" id=\"toc\">\n");
        s.push_str("    <h1>Table of Contents</h1>\n");
        if let Some(ol) = &self.nav_ol_html {
            s.push_str(ol);
        } else if let Some(first) = self.spine.first() {
            // Fallback: single entry pointing at the first spine chapter
            // (mirrors what `generate_ncx` does when `ncx_navmap` is unset).
            let href = self
                .manifest_by_id
                .get(&first.id)
                .map(|&idx| self.manifest[idx].href.clone())
                .unwrap_or_default();
            s.push_str(&format!(
                "    <ol>\n      <li><a href=\"{}\">{}</a></li>\n    </ol>\n",
                xml_escape(&href),
                xml_escape(title),
            ));
        } else {
            s.push_str("    <ol></ol>\n");
        }
        s.push_str("  </nav>\n");

        // Page-list nav (`<nav epub:type="page-list">`) — printed page numbers →
        // positions, round-tripped from the source KFX's `page_list` container.
        // Emitted only when present; `hidden` like the landmarks nav so it
        // drives "go to page N" without cluttering the visible TOC.
        if let Some(ol) = &self.page_list_ol_html {
            s.push_str("  <nav epub:type=\"page-list\" id=\"page-list\" hidden=\"\">\n");
            s.push_str("    <h2>List of Pages</h2>\n");
            s.push_str(ol);
            s.push_str("  </nav>\n");
        }

        // Landmarks nav, derived from the same `self.guide` source the
        // EPUB-2 `<guide>` block uses. EPUB-3 vocabulary differs from
        // EPUB-2 guide types in a few names (start → bodymatter,
        // acknowledgements vs acknowledgments); map at emit time so the
        // GuideRef struct stays a single source of truth.
        if !self.guide.is_empty() {
            s.push_str("  <nav epub:type=\"landmarks\" id=\"landmarks\" hidden=\"\">\n");
            s.push_str("    <h2>Landmarks</h2>\n");
            s.push_str("    <ol>\n");
            // EPUB 3 forbids two landmarks that share an epub:type AND reference
            // the same resource (epubcheck RSC-005). boko's own EPUB→KFX emits
            // both an `srl` (start-reading) and a `bodymatter` landmark for the
            // book's opening — which map to the same `bodymatter` + href here —
            // so keep the first of any (type, href) pair and drop later repeats.
            let mut seen_landmarks: std::collections::HashSet<(String, String)> =
                std::collections::HashSet::new();
            for g in &self.guide {
                let epub_type = guide_type_to_epub3(&g.guide_type);
                if !seen_landmarks.insert((epub_type.to_string(), g.href.clone())) {
                    continue;
                }
                // EPUB 3 requires every `<nav>` anchor to carry text (RSC-005);
                // KFX landmark containers sometimes yield an empty label (the
                // bodymatter/cover start marker), so fall back to a default.
                let label = if g.label.trim().is_empty() {
                    landmark_default_label(epub_type)
                } else {
                    g.label.as_str()
                };
                s.push_str(&format!(
                    "      <li><a epub:type=\"{}\" href=\"{}\">{}</a></li>\n",
                    epub_type,
                    xml_escape(&g.href),
                    xml_escape(label),
                ));
            }
            s.push_str("    </ol>\n");
            s.push_str("  </nav>\n");
        }

        s.push_str("</body>\n</html>\n");
        s
    }

    fn generate_ncx(&self, meta: &BookMetadata) -> String {
        let title = if meta.title.is_empty() {
            "Untitled"
        } else {
            &meta.title
        };
        let id = if meta.identifier.is_empty() {
            "urn:uuid:00000000-0000-0000-0000-000000000000"
        } else {
            &meta.identifier
        };
        let mut s = String::new();
        s.push_str(&format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE ncx PUBLIC "-//NISO//DTD ncx 2005-1//EN" "http://www.daisy.org/z3986/2005/ncx-2005-1.dtd">
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head>
    <meta name="dtb:uid" content="{id}"/>
    <meta name="dtb:depth" content="1"/>
    <meta name="dtb:totalPageCount" content="0"/>
    <meta name="dtb:maxPageNumber" content="0"/>
  </head>
  <docTitle><text>{title}</text></docTitle>
  <navMap>
"#,
            id = xml_escape(id),
            title = xml_escape(title)
        ));

        // Use the navigation module's NCX if provided; otherwise emit the
        // single-entry fallback pointing at the first spine chapter.
        if let Some(navmap) = &self.ncx_navmap {
            s.push_str(navmap);
        } else if let Some(first) = self.spine.first() {
            let href = self
                .manifest_by_id
                .get(&first.id)
                .map(|&idx| self.manifest[idx].href.clone())
                .unwrap_or_default();
            s.push_str(&format!(
                "    <navPoint id=\"navPoint-1\" playOrder=\"1\">\n      <navLabel><text>{}</text></navLabel>\n      <content src=\"{}\"/>\n    </navPoint>\n",
                xml_escape(title),
                xml_escape(&href)
            ));
        }

        s.push_str("  </navMap>\n</ncx>\n");
        s
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

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn io_error<E: std::error::Error + Send + Sync + 'static>(e: E) -> std::io::Error {
    std::io::Error::other(e)
}

/// Map an EPUB 2.0 `<guide>` reference type to the EPUB 3 nav-doc
/// `epub:type` vocabulary. Most types are identical; a few names differ
/// (`start` → `bodymatter`, `acknowledgements` → `acknowledgments`).
/// Unknown types pass through verbatim — readers ignore unknown values.
fn guide_type_to_epub3(guide_type: &str) -> &str {
    match guide_type {
        "start" | "text" => "bodymatter",
        "acknowledgements" => "acknowledgments",
        other => other,
    }
}

/// True if `xml` contains a real element `<name…>` (open tag), used to compute
/// EPUB-3 manifest `properties` (svg / mathml / scripted) for OPF-014. Matches
/// `<name` followed by a tag delimiter so `<svgfoo` doesn't count; text-node
/// `<` is `&lt;`-escaped in XHTML, so any raw `<name` is a genuine element.
fn contains_element(xml: &str, name: &str) -> bool {
    let needle = format!("<{name}");
    let mut hay = xml;
    while let Some(pos) = hay.find(&needle) {
        let after = pos + needle.len();
        if hay[after..]
            .chars()
            .next()
            .is_none_or(|c| matches!(c, ' ' | '\t' | '\r' | '\n' | '>' | '/'))
        {
            return true;
        }
        hay = &hay[after..];
    }
    false
}

/// Human-readable fallback label for a landmark whose KFX source carried no
/// text. EPUB 3 rejects an empty `<nav>` anchor (RSC-005 "Anchors within nav
/// elements must contain text"), so every landmark link needs a label.
fn landmark_default_label(epub_type: &str) -> &'static str {
    match epub_type {
        "cover" => "Cover",
        "toc" => "Table of Contents",
        "frontmatter" => "Front Matter",
        "backmatter" => "Back Matter",
        "loi" => "List of Illustrations",
        "lot" => "List of Tables",
        "preface" => "Preface",
        "bibliography" => "Bibliography",
        "index" => "Index",
        "glossary" => "Glossary",
        // "bodymatter" and anything unrecognized: the reading-start marker.
        _ => "Start of Content",
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

// `uuid_v5` lives in `crate::util` — shared with the generic exporter and
// the Aozora builder.
