//! EPUB writer for the mechanical port.
//!
//! Mirrors calibre's `EPUB_Output` minimally: keeps an ordered manifest, an
//! ordered spine, a flat map of OEBPS files, and emits a valid EPUB3 zip on
//! `finalize`. The full calibre class is ~1.5K LOC and does a lot more
//! (NCX/nav3, beautify, dedupe, viewport, cover-detection); we add those
//! pieces as later phase 1 steps need them.

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

/// EPUB output state shared across phase 1 steps.
pub struct EpubOutput {
    /// File path under `OEBPS/` → bytes. Insertion order tracked separately.
    oebps_files: HashMap<String, OebpsFile>,
    /// Insertion order of `oebps_files` keys.
    oebps_order: Vec<String>,

    /// Manifest entries, insertion order.
    manifest: Vec<ManifestEntry>,
    /// `id` → index in `manifest` for fast lookup / linking.
    manifest_by_id: HashMap<String, usize>,

    /// Spine: ordered list of manifest item ids.
    spine: Vec<String>,

    /// NCX navMap content (the inner <navPoint>…</navPoint> sequence).
    /// When set, `generate_ncx` uses this instead of the spine-derived
    /// fallback.
    pub ncx_navmap: Option<String>,

    /// Page-progression-direction. Mirrors calibre's `EPUB_Output.
    /// page_progression_direction`. Emitted to `<spine
    /// page-progression-direction="...">` only when set and not `"ltr"`
    /// (calibre suppresses the attribute for the EPUB default — `ltr`).
    pub page_progression_direction: Option<String>,

    /// Book-level writing mode (e.g. `vertical-rl`). When set and not
    /// `horizontal-tb`, the OPF emits `<meta name="primary-writing-mode">`
    /// as a Kindle reader hint — mirrors calibre's `epub_output.py:955+`.
    pub writing_mode: Option<String>,
}

impl EpubOutput {
    pub fn new() -> Self {
        Self {
            oebps_files: HashMap::new(),
            oebps_order: Vec::new(),
            manifest: Vec::new(),
            manifest_by_id: HashMap::new(),
            spine: Vec::new(),
            ncx_navmap: None,
            page_progression_direction: None,
            writing_mode: None,
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
        let new_id_str = new_id.map(|s| s.to_string()).unwrap_or_else(|| self.make_id(new_filename));
        let idx = *self.manifest_by_id.get(&old_id)?;
        self.manifest[idx].href = new_filename.to_string();
        self.manifest[idx].id = new_id_str.clone();
        // Re-index the lookup map.
        self.manifest_by_id.remove(&old_id);
        self.manifest_by_id.insert(new_id_str.clone(), idx);
        // Spine entries are by id — update any reference to the old id.
        for slot in &mut self.spine {
            if slot == &old_id {
                *slot = new_id_str.clone();
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
        self.spine.insert(0, id);
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

    /// Append a chapter `xhtml` to the spine, bundled at `OEBPS/<filename>`.
    pub fn add_spine_chapter(&mut self, filename: &str, xhtml: String) {
        let id = self.add_resource(
            filename,
            xhtml.into_bytes(),
            "application/xhtml+xml",
            None,
            None,
        );
        self.spine.push(id);
    }

    /// Finalize: build the EPUB zip in memory.
    pub fn finalize(&self, meta: &BookMetadata) -> std::io::Result<Vec<u8>> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut buf);
            let stored =
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
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

            // 4. toc.ncx (minimal — phase 1 step 2 will replace with real)
            let ncx = self.generate_ncx(meta);
            zip.start_file("OEBPS/toc.ncx", deflated).map_err(io_error)?;
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
        // EPUB 2.0 package. Reason: EPUB 3.0 mandates a nav.xhtml document
        // (`<item properties="nav">` in the manifest, distinct from NCX) and
        // strict readers — notably Apple Books — reject 3.0 packages lacking
        // one with no rendered output. Calibre's EPUB output uses 2.0 + NCX
        // for the same reason. We keep our existing OPF features that are
        // valid in both (ASIN identifiers, `<dc:date>`, `xml:lang`, custom
        // `<meta name="...">` hints, `page-progression-direction` on spine)
        // but drop the EPUB-3-only bits (`properties=` on manifest items,
        // `<meta property="dcterms:modified">`).
        let mut s = String::new();
        s.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="BookId">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
"#);

        // Title
        let title = if meta.title.is_empty() { "Untitled" } else { &meta.title };
        s.push_str(&format!("    <dc:title>{}</dc:title>\n", xml_escape(title)));

        // Authors — `opf:role="aut"` + a shared `opf:file-as` containing all
        // authors joined by ` & ` (calibre's convention; same string on every
        // creator). Lets EPUB libraries sort multi-author books consistently.
        let file_as = meta.authors.join(" & ");
        for author in &meta.authors {
            s.push_str(&format!(
                "    <dc:creator opf:file-as=\"{}\" opf:role=\"aut\">{}</dc:creator>\n",
                xml_escape(&file_as),
                xml_escape(author)
            ));
        }

        // Language
        let lang = if meta.language.is_empty() { "en" } else { &meta.language };
        s.push_str(&format!("    <dc:language>{}</dc:language>\n", xml_escape(lang)));

        // Identifier — calibre emits multiple <dc:identifier> with
        // opf:scheme="ASIN" / "MOBI-ASIN" / "uuid" / "calibre"; we mirror the
        // ASIN ones when present and use the KFX book_id as the unique-id.
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
                "    <dc:identifier opf:scheme=\"ASIN\">{}</dc:identifier>\n",
                xml_escape(asin)
            ));
            s.push_str(&format!(
                "    <dc:identifier opf:scheme=\"MOBI-ASIN\">{}</dc:identifier>\n",
                xml_escape(asin)
            ));
        }
        // Reproducible UUID v5 derived from the KFX book_id. Calibre emits a
        // randomly-generated UUID here; we use a deterministic one so two
        // converts of the same KFX produce the same OPF identifier.
        s.push_str(&format!(
            "    <dc:identifier opf:scheme=\"uuid\">{}</dc:identifier>\n",
            uuid_v5_from(id)
        ));

        // (EPUB3-only `<meta property="dcterms:modified">` intentionally
        // dropped — EPUB2 doesn't require it, and including it under EPUB2
        // is invalid because `property=` is an EPUB3 attribute.)

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
            s.push_str(&format!(
                "    <dc:date>{}</dc:date>\n",
                xml_escape(&iso)
            ));
        }

        // Publisher (optional)
        if let Some(ref pub_) = meta.publisher {
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

        // Primary writing mode hint — calibre emits this for any
        // non-`horizontal-tb` book (epub_output.py:955). The Kindle reader
        // uses it as a layout signal even though EPUB-3 readers also read
        // the CSS writing-mode declaration in the stylesheet.
        if let Some(wm) = self
            .writing_mode
            .as_deref()
            .filter(|v| !v.is_empty() && *v != "horizontal-tb")
        {
            s.push_str(&format!(
                "    <meta name=\"primary-writing-mode\" content=\"{}\"/>\n",
                xml_escape(wm)
            ));
        }

        s.push_str("  </metadata>\n");

        // Manifest
        s.push_str("  <manifest>\n");
        s.push_str(
            "    <item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n",
        );
        for m in &self.manifest {
            // EPUB-3-only `properties="cover-image"` / `properties="nav"`
            // intentionally omitted under EPUB 2.0. The cover marker lives
            // in `<meta name="cover" content="..."/>` instead (emitted in
            // the metadata block above), which is the EPUB2 convention all
            // readers (including Apple Books) honour.
            s.push_str(&format!(
                "    <item id=\"{}\" href=\"{}\" media-type=\"{}\"/>\n",
                xml_escape(&m.id),
                xml_escape(&m.href),
                xml_escape(&m.media_type),
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
        for id in &self.spine {
            s.push_str(&format!(
                "    <itemref idref=\"{}\"/>\n",
                xml_escape(id)
            ));
        }
        s.push_str("  </spine>\n");

        s.push_str("</package>\n");
        s
    }

    fn generate_ncx(&self, meta: &BookMetadata) -> String {
        let title = if meta.title.is_empty() { "Untitled" } else { &meta.title };
        let id = if meta.identifier.is_empty() {
            "urn:uuid:00000000-0000-0000-0000-000000000000"
        } else {
            &meta.identifier
        };
        let mut s = String::new();
        s.push_str(&format!(r#"<?xml version="1.0" encoding="UTF-8"?>
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
"#, id = xml_escape(id), title = xml_escape(title)));

        // Use the navigation module's NCX if provided; otherwise emit the
        // single-entry fallback pointing at the first spine chapter.
        if let Some(navmap) = &self.ncx_navmap {
            s.push_str(navmap);
        } else if let Some(first_chapter_id) = self.spine.first() {
            let href = self
                .manifest_by_id
                .get(first_chapter_id)
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

/// RFC 4122 v5 UUID derived from the KFX book identifier. SHA-1(namespace +
/// name), then set the version (5) and variant (RFC 4122) bits. Namespace is
/// the URL namespace UUID (6ba7b811-9dad-11d1-80b4-00c04fd430c8).
fn uuid_v5_from(name: &str) -> String {
    const URL_NAMESPACE: [u8; 16] = [
        0x6b, 0xa7, 0xb8, 0x11, 0x9d, 0xad, 0x11, 0xd1,
        0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
    ];
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(&URL_NAMESPACE);
    hasher.update(name.as_bytes());
    let digest = hasher.digest().bytes();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // Set version (5) in the high nibble of byte 6.
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    // Set variant (10xx) in the high nibble of byte 8.
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}
