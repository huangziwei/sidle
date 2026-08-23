//! Image-preservation validation — verify that every `<img src>` in the
//! source EPUB has a corresponding image resource and a storyline reference
//! in the converted KFX.
//!
//! KFX represents images with three pieces:
//!
//! 1. **`external_resource` ($164)** — metadata: `resource_name`, `location`
//!    (path to bytes), `format` (Png/Jpg/Gif/Webp/Bmp/Svg), `mime`, optional
//!    `resource_width`/`resource_height`.
//! 2. **`bcRawMedia` ($417)** — the raw bytes, named `resource/<resource_name>`.
//! 3. **Storyline element** with `type: image` and `resource_name` pointing
//!    at an external_resource. This is what actually renders.
//!
//! Without all three, the image either doesn't load, doesn't render, or
//! produces a phantom resource entry. The validator catches:
//!
//! - **dropped images** — source has more `<img>` than KFX has image
//!   elements in storylines.
//! - **dangling external_resource** — metadata exists but the `bcRawMedia`
//!   entity it points at is missing.
//! - **orphan image refs** — storyline references a `resource_name` with no
//!   matching `external_resource`.
//! - **orphan raw media** — `bcRawMedia` bytes that no `external_resource`
//!   points at (a wasted entity, but not user-visible).
//!
//! Image and font resources both use `external_resource`; this validator
//! filters by `format` symbol to image formats only (Png/Jpg/Gif/Webp/Bmp/
//! Svg) so it doesn't flag fonts.

use crate::formats::epub::structure::resolve_href;
use std::collections::HashSet;
use std::io::Cursor;

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

use crate::formats::epub::{parse_container_xml, parse_opf};
use crate::formats::kfx::container::{
    SymbolTable, parse_container_header, parse_container_info, parse_entity, parse_index_table,
    slice_at,
};
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::symbols::KfxSymbol;

/// A single `<img src>` from the EPUB side. May represent either the source
/// of an EPUB→KFX conversion or bokai's EPUB output in a KFX→EPUB conversion.
#[derive(Debug, Clone)]
pub struct EpubImage {
    pub spine_path: String,
    pub raw_src: String,
    /// `raw_src` resolved against `spine_path`, normalised and percent-decoded
    /// to match a zip entry name: `../images/cover.jpg` → `OEBPS/images/
    /// cover.jpg`. `None` for a src with a scheme (`http:`, `data:`).
    pub resolved_path: Option<String>,
    /// Whether `resolved_path` is present as an entry in the EPUB zip. False
    /// for external URLs and for tags whose referenced bytes weren't bundled.
    pub bundled: bool,
    /// Recognised image format ("jpeg", "png", "gif", "webp", "bmp", "svg")
    /// from the bundled bytes' magic. `None` covers bytes matching no image
    /// format, an undecoded `external_resource` Ion struct among them.
    pub detected_format: Option<String>,
}

/// Classify the leading bytes of a bundled file. Returns the standard format
/// name (lowercase) or `None` if the magic doesn't match.
fn detect_image_format(bytes: &[u8]) -> Option<String> {
    if bytes.len() >= 3 && bytes[..3] == [0xFF, 0xD8, 0xFF] {
        return Some("jpeg".into());
    }
    if bytes.len() >= 8 && bytes[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some("png".into());
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return Some("gif".into());
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp".into());
    }
    if bytes.len() >= 2 && &bytes[..2] == b"BM" {
        return Some("bmp".into());
    }
    // SVG: text/XML. Look for either `<?xml` (with svg later) or `<svg` early.
    if bytes.len() >= 5 {
        let head: String = bytes.iter().take(1024).map(|&b| b as char).collect();
        let lower = head.to_ascii_lowercase();
        if lower.starts_with("<?xml") && lower.contains("<svg") {
            return Some("svg".into());
        }
        if lower.trim_start().starts_with("<svg") {
            return Some("svg".into());
        }
    }
    None
}

/// One KFX `external_resource` entity, filtered to image formats.
#[derive(Debug, Clone)]
pub struct ExternalResource {
    pub resource_name: String,
    /// `location` field — should equal `resource/<resource_name>` and refer
    /// to a `bcRawMedia` entity that exists.
    pub location: String,
    /// `format` symbol name (e.g. `$$png`, `$$jpg`).
    pub format: String,
}

#[derive(Debug, Clone)]
pub struct DanglingResource {
    pub resource_name: String,
    pub location: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub epub_images: Vec<EpubImage>,
    pub kfx_external_resources: Vec<ExternalResource>,
    /// Names of every `bcRawMedia` entity in the container.
    pub kfx_raw_media_names: HashSet<String>,
    /// Distinct `resource_name` symbols referenced by storyline image elements.
    pub kfx_image_refs_distinct: HashSet<String>,
    /// Total count of storyline image elements (one per `type: image`).
    pub kfx_image_element_count: usize,

    // --- Counts ---
    pub epub_image_count: usize,
    pub epub_distinct_srcs: usize,
    pub kfx_image_resource_count: usize,
    /// Count of `<img src>` whose referenced bytes are present as an entry in
    /// the EPUB zip AND look like a valid image format (JPEG/PNG/GIF/WebP/
    /// BMP/SVG by magic number).
    pub epub_renderable_image_count: usize,
    /// Count of `<img src>` whose resolved path doesn't exist in the bundle.
    pub epub_missing_image_count: usize,
    /// Count of `<img src>` whose resolved path exists over bytes matching no
    /// image magic, an undecoded KFX Ion resource struct among them.
    pub epub_unreadable_image_count: usize,

    // --- Defects (EPUB-side internal) ---
    /// EPUB images whose `resolved_path` doesn't exist in the bundle.
    pub epub_missing_images: Vec<EpubImage>,
    /// EPUB images whose bytes exist but aren't a recognised image format.
    pub epub_unreadable_images: Vec<EpubImage>,

    // --- Defects (KFX-side internal) ---
    /// `external_resource` entities whose `location` doesn't refer to an
    /// existing `bcRawMedia` entity.
    pub dangling_external_resources: Vec<DanglingResource>,
    /// Storyline image elements whose `resource_name` has no `external_resource`.
    pub orphan_image_refs: Vec<String>,
    /// `bcRawMedia` names that no `external_resource.location` points at.
    /// Not a user-visible defect, but signals dead bytes in the file.
    pub orphan_raw_media: Vec<String>,
}

impl Report {
    /// Whether every internal `<img>` in the EPUB resolves to bundled bytes
    /// that parse as an image. `dangling_external_resources`, `orphan_image_refs`,
    /// `orphan_raw_media` and the image counts print as informational.
    pub fn is_clean(&self) -> bool {
        self.epub_missing_image_count == 0 && self.epub_unreadable_image_count == 0
    }

    /// Count of images dropped by bokai's converter, given the conversion
    /// direction. `max(0, source_count - target_count)`.
    pub fn dropped_count(&self, dir: super::Direction) -> usize {
        if dir.epub_is_source() {
            self.epub_image_count
                .saturating_sub(self.kfx_image_element_count)
        } else {
            self.kfx_image_element_count
                .saturating_sub(self.epub_image_count)
        }
    }

    pub fn preservation_ratio(&self, dir: super::Direction) -> f64 {
        let source_count = if dir.epub_is_source() {
            self.epub_image_count
        } else {
            self.kfx_image_element_count
        };
        if source_count == 0 {
            return 1.0;
        }
        let preserved = source_count.saturating_sub(self.dropped_count(dir));
        // For KFX→EPUB, also subtract missing + unreadable EPUB images — the
        // <img> tag exists but the reader can't actually display anything.
        let preserved = if !dir.epub_is_source() {
            preserved
                .saturating_sub(self.epub_missing_image_count)
                .saturating_sub(self.epub_unreadable_image_count)
        } else {
            preserved
        };
        preserved as f64 / source_count as f64
    }

    pub fn print_summary(&self, dir: super::Direction) {
        println!("EPUB images:");
        println!(
            "  <img>:         {} ({} distinct src values)",
            self.epub_image_count, self.epub_distinct_srcs
        );
        println!(
            "  bundled bytes: {} renderable, {} unreadable, {} missing",
            self.epub_renderable_image_count,
            self.epub_unreadable_image_count,
            self.epub_missing_image_count
        );
        println!("KFX images:");
        println!(
            "  external_resource (image format): {}",
            self.kfx_image_resource_count
        );
        println!(
            "  bcRawMedia entities:              {}",
            self.kfx_raw_media_names.len()
        );
        println!(
            "  storyline image elements:         {} ({} distinct refs)",
            self.kfx_image_element_count,
            self.kfx_image_refs_distinct.len()
        );
        println!("Defects:");
        println!(
            "  dropped images ({} - {}): {}",
            dir.source_label(),
            dir.target_label(),
            self.dropped_count(dir)
        );
        println!(
            "  EPUB <img> missing bytes:   {}",
            self.epub_missing_image_count
        );
        println!(
            "  EPUB <img> unreadable bytes: {} (e.g. raw KFX Ion blob, JXR not re-encoded)",
            self.epub_unreadable_image_count
        );
        println!(
            "  dangling external_resource:    {}",
            self.dangling_external_resources.len()
        );
        println!(
            "  orphan storyline image refs:   {}",
            self.orphan_image_refs.len()
        );
        println!(
            "  orphan bcRawMedia entities:    {}",
            self.orphan_raw_media.len()
        );
    }

    pub fn print_details(&self, limit: usize) {
        if !self.epub_missing_images.is_empty() {
            println!(
                "\n--- EPUB <img src> pointing at missing zip entries [first {}] ---",
                limit
            );
            for img in self.epub_missing_images.iter().take(limit) {
                let resolved = img.resolved_path.as_deref().unwrap_or("(unresolved)");
                println!(
                    "  {}  src={:?}  →  {}",
                    img.spine_path, img.raw_src, resolved
                );
            }
            if self.epub_missing_images.len() > limit {
                println!("  ... and {} more", self.epub_missing_images.len() - limit);
            }
        }
        if !self.epub_unreadable_images.is_empty() {
            println!(
                "\n--- EPUB <img src> with unreadable bytes (no image magic) [first {}] ---",
                limit
            );
            for img in self.epub_unreadable_images.iter().take(limit) {
                let resolved = img.resolved_path.as_deref().unwrap_or("(unresolved)");
                println!(
                    "  {}  src={:?}  →  {}",
                    img.spine_path, img.raw_src, resolved
                );
            }
            if self.epub_unreadable_images.len() > limit {
                println!(
                    "  ... and {} more",
                    self.epub_unreadable_images.len() - limit
                );
            }
        }
        if !self.dangling_external_resources.is_empty() {
            println!(
                "\n--- external_resource pointing at missing bcRawMedia [first {}] ---",
                limit
            );
            for d in self.dangling_external_resources.iter().take(limit) {
                println!("  {}  →  {}", d.resource_name, d.location);
            }
            if self.dangling_external_resources.len() > limit {
                println!(
                    "  ... and {} more",
                    self.dangling_external_resources.len() - limit
                );
            }
        }
        if !self.orphan_image_refs.is_empty() {
            println!(
                "\n--- Storyline image refs with no external_resource [first {}] ---",
                limit
            );
            for name in self.orphan_image_refs.iter().take(limit) {
                println!("  {}", name);
            }
            if self.orphan_image_refs.len() > limit {
                println!("  ... and {} more", self.orphan_image_refs.len() - limit);
            }
        }
        if !self.orphan_raw_media.is_empty() {
            println!(
                "\n--- bcRawMedia not referenced by any external_resource [first {}] ---",
                limit
            );
            for name in self.orphan_raw_media.iter().take(limit) {
                println!("  {}", name);
            }
            if self.orphan_raw_media.len() > limit {
                println!("  ... and {} more", self.orphan_raw_media.len() - limit);
            }
        }
    }
}

pub fn validate(epub_bytes: &[u8], kfx_bytes: &[u8]) -> Result<Report, String> {
    let epub_images = extract_images_from_epub(epub_bytes)?;
    let kfx = extract_image_data_from_kfx(kfx_bytes)?;

    let distinct_srcs: usize = {
        let mut set: HashSet<&str> = HashSet::new();
        for img in &epub_images {
            set.insert(&img.raw_src);
        }
        set.len()
    };

    // dangling external_resource: location must refer to an existing bcRawMedia
    let mut dangling_external_resources: Vec<DanglingResource> = Vec::new();
    let mut referenced_raw_media: HashSet<String> = HashSet::new();
    for r in &kfx.external_resources {
        if kfx.raw_media_names.contains(&r.location) {
            referenced_raw_media.insert(r.location.clone());
        } else {
            dangling_external_resources.push(DanglingResource {
                resource_name: r.resource_name.clone(),
                location: r.location.clone(),
            });
        }
    }

    // orphan raw media: bcRawMedia names not referenced by any external_resource
    let mut orphan_raw_media: Vec<String> = kfx
        .raw_media_names
        .iter()
        .filter(|n| !referenced_raw_media.contains(*n))
        .cloned()
        .collect();
    orphan_raw_media.sort();

    // orphan image refs: storyline resource_name with no external_resource
    let resource_names: HashSet<&str> = kfx
        .external_resources
        .iter()
        .map(|r| r.resource_name.as_str())
        .collect();
    let mut orphan_image_refs: Vec<String> = kfx
        .image_refs_distinct
        .iter()
        .filter(|n| !resource_names.contains(n.as_str()))
        .cloned()
        .collect();
    orphan_image_refs.sort();

    let epub_image_count = epub_images.len();

    // Bundled-bytes accounting over references with a resolved path, external
    // URLs excluded: renderable is bundled with image magic, unreadable is
    // bundled without, missing is absent from the zip.
    let mut epub_renderable_image_count = 0;
    let mut epub_missing_image_count = 0;
    let mut epub_unreadable_image_count = 0;
    let mut epub_missing_images: Vec<EpubImage> = Vec::new();
    let mut epub_unreadable_images: Vec<EpubImage> = Vec::new();
    for img in &epub_images {
        if img.resolved_path.is_none() {
            continue;
        }
        if !img.bundled {
            epub_missing_image_count += 1;
            epub_missing_images.push(img.clone());
        } else if img.detected_format.is_some() {
            epub_renderable_image_count += 1;
        } else {
            epub_unreadable_image_count += 1;
            epub_unreadable_images.push(img.clone());
        }
    }

    Ok(Report {
        epub_images,
        epub_image_count,
        epub_distinct_srcs: distinct_srcs,
        kfx_image_resource_count: kfx.external_resources.len(),
        kfx_external_resources: kfx.external_resources,
        kfx_raw_media_names: kfx.raw_media_names,
        kfx_image_refs_distinct: kfx.image_refs_distinct,
        kfx_image_element_count: kfx.image_element_count,
        epub_renderable_image_count,
        epub_missing_image_count,
        epub_unreadable_image_count,
        epub_missing_images,
        epub_unreadable_images,
        dangling_external_resources,
        orphan_image_refs,
        orphan_raw_media,
    })
}

// ============================================================================
// Source-side extraction
// ============================================================================

pub fn extract_images_from_epub(epub_bytes: &[u8]) -> Result<Vec<EpubImage>, String> {
    let cursor = Cursor::new(epub_bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|e| format!("not a valid zip: {}", e))?;

    let container_bytes = read_zip_entry(&mut archive, "META-INF/container.xml")
        .map_err(|e| format!("container.xml: {}", e))?;
    let opf_path = parse_container_xml(&container_bytes)
        .map_err(|e| format!("container.xml parse: {:?}", e))?;
    let opf_base = opf_path
        .rfind('/')
        .map(|i| &opf_path[..=i])
        .unwrap_or("")
        .to_string();

    let opf_bytes =
        read_zip_entry(&mut archive, &opf_path).map_err(|e| format!("opf {}: {}", opf_path, e))?;
    let hint_encoding = crate::util::extract_xml_encoding(&opf_bytes);
    let opf_str = crate::util::decode_text(&opf_bytes, hint_encoding);
    let opf = parse_opf(&opf_str).map_err(|e| format!("opf parse: {:?}", e))?;

    // Every zip entry name, the set each `<img src>` is checked against: an
    // emitted `<img>` whose bytes were never bundled is absent from it.
    let zip_entries: HashSet<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|f| f.name().to_string()))
        .collect();

    let mut images = Vec::new();
    for spine_id in &opf.spine_ids {
        let Some((href, _media_type)) = opf.manifest.get(spine_id) else {
            continue;
        };
        let full_path = resolve_href(&opf_base, href);
        let Ok(xhtml_bytes) = read_zip_entry(&mut archive, &full_path) else {
            continue;
        };
        let enc = crate::util::extract_xml_encoding(&xhtml_bytes);
        let xhtml = crate::util::decode_text(&xhtml_bytes, enc);
        extract_images_from_xhtml(&xhtml, &full_path, &mut images);
    }

    // Each src resolved against its spine path, with the leading bytes of a
    // bundled entry read for its format. `head_cache` holds one read per path.
    let mut head_cache: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    for img in &mut images {
        let (resolved, bundled) = resolve_against_zip(&img.spine_path, &img.raw_src, &zip_entries);
        img.resolved_path = resolved.clone();
        img.bundled = bundled;
        if !bundled {
            continue;
        }
        let Some(path) = resolved else { continue };
        let detected = head_cache.entry(path.clone()).or_insert_with(|| {
            // `path` is the decoded form, the only name tried against the zip.
            let head_bytes = read_zip_head(&mut archive, &path).unwrap_or_default();
            detect_image_format(&head_bytes)
        });
        img.detected_format = detected.clone();
    }

    Ok(images)
}

/// Read up to 1024 bytes from a zip entry, or None on error.
fn read_zip_head<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut file = archive.by_name(name).ok()?;
    let mut buf = vec![0u8; 1024.min(file.size() as usize)];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// An href resolved against the spine path's base directory, `..`/`.`
/// normalised and percent-decoded, with whether it names a zip entry.
/// `(None, false)` for an href carrying a scheme.
fn resolve_against_zip(
    spine_path: &str,
    raw_src: &str,
    zip_entries: &HashSet<String>,
) -> (Option<String>, bool) {
    let trimmed = raw_src.split('#').next().unwrap_or(raw_src).trim();
    if trimmed.is_empty() {
        return (None, false);
    }
    if has_url_scheme(trimmed) {
        return (None, false);
    }
    let base = match spine_path.rfind('/') {
        Some(i) => &spine_path[..=i],
        None => "",
    };
    let joined = if trimmed.starts_with('/') {
        trimmed.trim_start_matches('/').to_string()
    } else {
        format!("{}{}", base, trimmed)
    };
    let normalized = normalize_path(&joined);
    let decoded = crate::util::percent_decode(&normalized);
    let bundled = zip_entries.contains(&decoded) || zip_entries.contains(&normalized);
    (Some(decoded), bundled)
}

fn has_url_scheme(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        if b == b':' {
            return i >= 1;
        }
        if !(b.is_ascii_alphanumeric() || b == b'+' || b == b'.' || b == b'-') {
            return false;
        }
    }
    false
}

fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn read_zip_entry<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, std::io::Error> {
    use std::io::Read;
    let mut file = archive.by_name(name)?;
    let mut buf = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Collect `<img src>` from one XHTML. Also collects SVG `<image href>` and
/// `<image xlink:href>` since bokai emits the same Image role for both.
pub fn extract_images_from_xhtml(xhtml: &str, spine_path: &str, out: &mut Vec<EpubImage>) {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let local = e.local_name();
                let is_img = local.as_ref() == b"img";
                let is_svg_image = local.as_ref() == b"image";
                if !is_img && !is_svg_image {
                    continue;
                }
                for attr in e.attributes().flatten() {
                    // <img src>, <image href>, <image xlink:href>
                    let key = attr.key.local_name();
                    let key_bytes = key.as_ref();
                    let is_target = (is_img && key_bytes == b"src")
                        || (is_svg_image && (key_bytes == b"href" || key_bytes == b"xlink:href"));
                    if !is_target {
                        continue;
                    }
                    let src = String::from_utf8_lossy(&attr.value).into_owned();
                    if src.trim().is_empty() {
                        continue;
                    }
                    out.push(EpubImage {
                        spine_path: spine_path.to_string(),
                        raw_src: src,
                        resolved_path: None,
                        bundled: false,
                        detected_format: None,
                    });
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

// ============================================================================
// KFX-side extraction
// ============================================================================

#[derive(Debug, Default)]
pub struct KfxImageData {
    pub external_resources: Vec<ExternalResource>,
    pub raw_media_names: HashSet<String>,
    /// Distinct `resource_name` symbols referenced by storyline image elements.
    pub image_refs_distinct: HashSet<String>,
    /// Total count of storyline elements with `type: image`.
    pub image_element_count: usize,
}

pub fn extract_image_data_from_kfx(kfx_bytes: &[u8]) -> Result<KfxImageData, String> {
    let header = parse_container_header(kfx_bytes).map_err(|e| format!("kfx header: {:?}", e))?;
    let info_data = slice_at(
        kfx_bytes,
        header.container_info_offset,
        header.container_info_length,
    )
    .ok_or("container info out of bounds")?;
    let info =
        parse_container_info(info_data).map_err(|e| format!("kfx container info: {:?}", e))?;

    // SymbolTable::from_fragment seats doc-local ids at the container's
    // declared import max_id.
    let symbols = SymbolTable::from_fragment(
        info.doc_symbols
            .and_then(|(off, len)| slice_at(kfx_bytes, off, len)),
    );

    let resolve_sym = |id: u64| -> String { symbols.resolve(id).to_string() };

    let Some((idx_off, idx_len)) = info.index else {
        return Err("kfx: no index table".into());
    };
    let index_data = slice_at(kfx_bytes, idx_off, idx_len).ok_or("kfx: index out of bounds")?;
    let entities = parse_index_table(index_data, header.header_len);

    let external_resource_type = KfxSymbol::ExternalResource as u32;
    let bcrawmedia_type = KfxSymbol::Bcrawmedia as u32;
    let storyline_type = KfxSymbol::Storyline as u32;

    // An entity's `id: u32` is the symbol id of its fragment name. A
    // bcRawMedia name is `resource/<resource_name>`, equal to that resource's
    // `external_resource.location`.

    let mut external_resources: Vec<ExternalResource> = Vec::new();
    let mut raw_media_names: HashSet<String> = HashSet::new();
    let mut image_refs_distinct: HashSet<String> = HashSet::new();
    let mut image_element_count: usize = 0;

    for ent in &entities {
        if ent.type_id == external_resource_type {
            if let Some(value) = parse_entity(kfx_bytes, ent)
                && let Some(r) = extract_external_resource(&value, &resolve_sym)
                && is_image_format(&r.format)
            {
                external_resources.push(r);
            }
        } else if ent.type_id == bcrawmedia_type {
            let name = resolve_sym(ent.id as u64);
            if !name.is_empty() && name != "?" {
                raw_media_names.insert(name);
            }
        } else if ent.type_id == storyline_type
            && let Some(value) = parse_entity(kfx_bytes, ent)
        {
            walk_storyline_for_images(
                &value,
                &resolve_sym,
                &mut image_refs_distinct,
                &mut image_element_count,
            );
        }
    }

    Ok(KfxImageData {
        external_resources,
        raw_media_names,
        image_refs_distinct,
        image_element_count,
    })
}

fn extract_external_resource<F>(value: &IonValue, resolve_sym: &F) -> Option<ExternalResource>
where
    F: Fn(u64) -> String,
{
    let inner = match value {
        IonValue::Annotated(_, inner) => inner.as_ref(),
        _ => value,
    };
    let IonValue::Struct(fields) = inner else {
        return None;
    };

    let mut resource_name = String::new();
    let mut location = String::new();
    let mut format = String::new();

    for (k, v) in fields {
        match resolve_sym(*k).as_str() {
            "resource_name" => {
                if let IonValue::Symbol(s) = v {
                    resource_name = resolve_sym(*s);
                } else if let IonValue::String(s) = v {
                    resource_name = s.clone();
                }
            }
            "location" => {
                if let IonValue::String(s) = v {
                    location = s.clone();
                }
            }
            "format" => {
                if let IonValue::Symbol(s) = v {
                    format = resolve_sym(*s);
                }
            }
            _ => {}
        }
    }

    if resource_name.is_empty() {
        return None;
    }
    Some(ExternalResource {
        resource_name,
        location,
        format,
    })
}

fn is_image_format(format: &str) -> bool {
    // `jxr` (JPEG-XR) is the default interior-plate codec, and a fixed-layout
    // book carries every page in it.
    matches!(
        format,
        "$$png"
            | "$$jpg"
            | "$$gif"
            | "$$webp"
            | "$$bmp"
            | "$$svg"
            | "$$jxr"
            | "png"
            | "jpg"
            | "gif"
            | "webp"
            | "bmp"
            | "svg"
            | "jxr"
    )
}

/// Walk a storyline, count elements with `type: image`, and collect their
/// `resource_name` references.
fn walk_storyline_for_images<F>(
    value: &IonValue,
    resolve_sym: &F,
    refs: &mut HashSet<String>,
    count: &mut usize,
) where
    F: Fn(u64) -> String,
{
    match value {
        IonValue::Struct(fields) => {
            // Detect `type: image` on this struct.
            let mut is_image = false;
            for (k, v) in fields {
                if resolve_sym(*k) == "type"
                    && let IonValue::Symbol(s) = v
                    && resolve_sym(*s) == "image"
                {
                    is_image = true;
                }
            }
            if is_image {
                *count += 1;
                for (k, v) in fields {
                    if resolve_sym(*k) == "resource_name"
                        && let IonValue::Symbol(s) = v
                    {
                        let name = resolve_sym(*s);
                        if !name.is_empty() {
                            refs.insert(name);
                        }
                    }
                }
            }
            for (_, v) in fields {
                walk_storyline_for_images(v, resolve_sym, refs, count);
            }
        }
        IonValue::List(items) => {
            for item in items {
                walk_storyline_for_images(item, resolve_sym, refs, count);
            }
        }
        IonValue::Annotated(_, inner) => {
            walk_storyline_for_images(inner, resolve_sym, refs, count);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xhtml_collects_img() {
        let xhtml = r#"<html><body>
            <p>before</p>
            <img src="cover.jpg" alt="cover" />
            <img src="figures/fig1.png"/>
            <p>between</p>
            <img src="images/photo.webp" />
        </body></html>"#;
        let mut out = Vec::new();
        extract_images_from_xhtml(xhtml, "OEBPS/ch1.xhtml", &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].raw_src, "cover.jpg");
        assert_eq!(out[1].raw_src, "figures/fig1.png");
        assert_eq!(out[2].raw_src, "images/photo.webp");
    }

    #[test]
    fn xhtml_collects_svg_image() {
        let xhtml = r##"<html><body>
            <svg><image xlink:href="cover.jpg"/></svg>
        </body></html>"##;
        let mut out = Vec::new();
        extract_images_from_xhtml(xhtml, "OEBPS/ch1.xhtml", &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].raw_src, "cover.jpg");
    }

    #[test]
    fn xhtml_skips_empty_src() {
        let xhtml = r#"<html><body><img src=""/></body></html>"#;
        let mut out = Vec::new();
        extract_images_from_xhtml(xhtml, "OEBPS/ch1.xhtml", &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn image_format_detection() {
        assert!(is_image_format("$$png"));
        assert!(is_image_format("$$jpg"));
        assert!(is_image_format("png"));
        // JXR is bokai's default interior/manga codec — must count as an image.
        assert!(is_image_format("$$jxr"));
        assert!(is_image_format("jxr"));
        assert!(!is_image_format("$$ttf"));
        assert!(!is_image_format("woff2"));
    }
}
