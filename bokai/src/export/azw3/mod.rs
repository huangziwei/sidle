//! AZW3/KF8 exporter.
//!
//! Creates KF8 (Kindle Format 8) files from Book structures.

use std::collections::{HashMap, HashSet};
use std::io::{self, Seek, Write};

use flate2::Compression;
use flate2::write::ZlibEncoder;

use crate::export::epub::guess_media_type;
use crate::formats::mobi::index::{
    GuideBuildEntry, NcxBuildEntry, build_chunk_indx, build_cncx, build_guide_indx, build_ncx_indx,
    build_skel_indx, calculate_cncx_offsets,
};
use crate::formats::mobi::skeleton::{Chunker, ChunkerResult};
use crate::formats::mobi::writer_transform::{
    rewrite_css_references_fast, rewrite_html_references_fast, write_base32_4, write_base32_10,
};
use crate::model::{Book, Resource, TocEntry};

use super::Exporter;

// Constants
const RECORD_SIZE: usize = 4096;
const NULL_INDEX: u32 = 0xFFFF_FFFF;
const XOR_KEY_LEN: usize = 20;

mod guide;
mod kf8;

use kf8::Kf8Builder;

/// Configuration for AZW3 export.
#[derive(Debug, Clone, Default)]
pub struct Azw3Config {
    /// If true, normalize content through IR pipeline for clean, consistent output.
    /// Default is false (passthrough mode preserves original HTML/CSS).
    pub normalize: bool,
}

/// AZW3/KF8 format exporter.
///
/// Creates KF8 files compatible with modern Kindle devices.
pub struct Azw3Exporter {
    config: Azw3Config,
}

impl Azw3Exporter {
    /// Create a new exporter with default configuration.
    pub fn new() -> Self {
        Self {
            config: Azw3Config::default(),
        }
    }

    /// Configure the exporter with custom settings.
    pub fn with_config(mut self, config: Azw3Config) -> Self {
        self.config = config;
        self
    }
}

impl Default for Azw3Exporter {
    fn default() -> Self {
        Self::new()
    }
}

impl Exporter for Azw3Exporter {
    fn export<W: Write + Seek>(&self, book: &mut Book, writer: &mut W) -> io::Result<()> {
        // Normalize when explicitly requested OR when the source format requires
        // it (e.g. KFX raw content is binary Ion, not HTML) — otherwise the
        // builder would chunk and compress that binary as if it were XHTML.
        let normalize = self.config.normalize || book.requires_normalized_export();
        let builder = Kf8Builder::new(book, normalize)?;
        builder.write(writer)
    }
}

/// Internal context for collecting book data.
struct BookContext {
    /// Maps href -> Resource (data + media_type)
    resources: HashMap<String, Resource>,
    /// Spine items as (href, data) pairs
    spine: Vec<SpineItem>,
    /// TOC entries
    toc: Vec<TocEntry>,
    /// Metadata
    metadata: crate::model::Metadata,
    /// Landmarks (used to build the K8 guide index).
    landmarks: Vec<crate::model::Landmark>,
}

impl BookContext {
    fn landmarks(&self) -> &[crate::model::Landmark] {
        &self.landmarks
    }
}

/// A reading-order entry. Chapter bytes live in `BookContext::resources`
/// (keyed by href) — storing them here too doubled peak memory for the whole
/// text payload and read every spine document from the archive twice.
struct SpineItem {
    href: String,
}

impl BookContext {
    /// Collect all data from a Book into internal structures.
    fn from_book(book: &mut Book, normalize: bool) -> io::Result<Self> {
        if normalize {
            Self::from_normalized(book)
        } else {
            Self::from_raw(book)
        }
    }

    /// Collect raw (passthrough) content from the book.
    fn from_raw(book: &mut Book) -> io::Result<Self> {
        // Collect metadata, TOC, landmarks up front (immutable borrows, cloned)
        // so the mutable `load_raw`/`load_asset` calls below don't conflict.
        let metadata = book.metadata().clone();
        let toc = book.toc().to_vec();
        let landmarks = book.landmarks().to_vec();
        // Snapshot the spine ids: `spine()` borrows the book immutably, but
        // `load_raw` needs it mutable — collect the ids first, then load.
        let spine_ids: Vec<crate::import::ChapterId> = book.spine().iter().map(|e| e.id).collect();

        let mut spine = Vec::with_capacity(spine_ids.len());
        let mut resources = HashMap::new();

        for id in spine_ids {
            let href = book.source_id(id).unwrap_or("unknown.xhtml").to_string();
            let data = book.load_raw(id)?;
            // Guess from the extension so a non-XHTML spine item (SVG-in-spine
            // is legal EPUB) keeps its real type and is routed to resource
            // records, not the text/chunker pipeline; fall back to XHTML only
            // for unknown/extensionless names.
            let media_type = match guess_media_type(&href).as_str() {
                "application/octet-stream" => "application/xhtml+xml".to_string(),
                _ => guess_media_type(&href),
            };
            resources.insert(href.clone(), Resource { data, media_type });
            spine.push(SpineItem { href });
        }

        // Collect assets, skipping spine documents already loaded above.
        // Snapshot the asset paths (owned) so the mutable `load_asset` below
        // doesn't conflict with the immutable `list_assets` borrow.
        let asset_paths: Vec<String> = book
            .list_assets()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        for path in asset_paths {
            if resources.contains_key(&path) {
                continue;
            }
            let data = book.load_asset(std::path::Path::new(&path))?;
            let media_type = guess_media_type(&path);
            resources.insert(path.clone(), Resource { data, media_type });
        }

        Ok(Self {
            resources,
            spine,
            toc,
            metadata,
            landmarks,
        })
    }

    /// Collect normalized content from the book through IR pipeline.
    fn from_normalized(book: &mut Book) -> io::Result<Self> {
        use crate::export::epub::normalize_book;

        // Immutable reads first (cloned), then the exclusive `normalize_book`
        // and `load_asset` mutable borrows.
        let metadata = book.metadata().clone();
        let mut toc = book.toc().to_vec();
        let mut landmarks = book.landmarks().to_vec();

        let normalized = normalize_book(book)?;

        // Spine filenames from source ids via the shared `chapter_filenames`
        // (the scheme `normalize_book` resolved the chapters' links against).
        // Computed once here — reused for the spine below and, first, to
        // resolve the TOC/landmark targets.
        let filenames = crate::export::epub::chapter_filenames(
            normalized.chapters.iter().map(|c| c.source_path.as_str()),
        );

        // Resolve TOC + landmark hrefs (`#eid[:offset]` placeholders) to
        // `file#frag` against the normalized spine, exactly as the EPUB nav
        // emitter does (shared `resolve_nav_href`), so `flatten_toc` /
        // `collect_guide_entries` resolve each entry to a distinct position
        // through the chunker's id_map. Without it the KFX TOC's bare `#eid`
        // hrefs miss id_map and every entry collapses onto pos 0 — one shared
        // href, unusable in-book navigation.
        if !toc.is_empty() || !landmarks.is_empty() {
            let spine_ids: Vec<crate::import::ChapterId> =
                book.spine().iter().map(|e| e.id).collect();
            let mut anchor_chapters = Vec::with_capacity(spine_ids.len());
            for &id in &spine_ids {
                anchor_chapters.push((id, book.load_chapter_cached(id)?));
            }
            book.index_anchors(&anchor_chapters);
            let chapter_pos: HashMap<crate::import::ChapterId, usize> = spine_ids
                .iter()
                .enumerate()
                .map(|(i, &id)| (id, i))
                .collect();
            resolve_toc_hrefs(&mut toc, book, &chapter_pos, &filenames);
            for lm in &mut landmarks {
                if let Some(h) = crate::export::epub::resolve_nav_href(
                    book,
                    &lm.href,
                    &chapter_pos,
                    &filenames,
                    false,
                ) {
                    lm.href = h;
                }
            }
        }

        let mut resources = HashMap::new();

        // Add unified CSS as a resource
        if !normalized.css.is_empty() {
            resources.insert(
                "style.css".to_string(),
                Resource {
                    data: normalized.css.into_bytes(),
                    media_type: "text/css".to_string(),
                },
            );
        }

        // Build spine from normalized chapters; bytes stored once in
        // `resources`. Files are named by `filenames` above (shared
        // `chapter_filenames`) — the SAME scheme `normalize_book` resolved the
        // chapters' internal links against (and the EPUB exporter emits), so
        // `<a href="{source}.xhtml">` targets match the spine hrefs and become
        // real pos:fid links instead of dangling `chapter_N.xhtml` names that
        // no file answers to (epubcheck RSC-007 on re-import).
        let mut spine = Vec::with_capacity(normalized.chapters.len());
        for (chapter, href) in normalized.chapters.iter().zip(&filenames) {
            resources.insert(
                href.clone(),
                Resource {
                    data: chapter.document.as_bytes().to_vec(),
                    media_type: "application/xhtml+xml".to_string(),
                },
            );
            spine.push(SpineItem { href: href.clone() });
        }

        // Add referenced assets
        for asset_path in &normalized.assets {
            if let Ok(data) = book.load_asset(std::path::Path::new(asset_path)) {
                let media_type = guess_media_type(asset_path);
                resources.insert(asset_path.clone(), Resource { data, media_type });
            }
        }

        Ok(Self {
            resources,
            spine,
            toc,
            metadata,
            landmarks,
        })
    }
}

/// Rewrite each TOC entry's `#eid[:offset]` placeholder href to its resolved
/// `file#frag` form (recursively over children) via the shared
/// [`resolve_nav_href`](crate::export::epub::resolve_nav_href), so the
/// chunker's id_map lookup in `flatten_toc` finds a distinct position per
/// entry. An entry whose target doesn't resolve keeps its original href —
/// `flatten_toc` then falls back to the chapter start, matching a label-only
/// node.
fn resolve_toc_hrefs(
    entries: &mut [TocEntry],
    book: &Book,
    chapter_pos: &HashMap<crate::import::ChapterId, usize>,
    chapter_files: &[String],
) {
    for entry in entries.iter_mut() {
        if let Some(href) = crate::export::epub::resolve_nav_href(
            book,
            &entry.href,
            chapter_pos,
            chapter_files,
            false,
        ) {
            entry.href = href;
        }
        resolve_toc_hrefs(&mut entry.children, book, chapter_pos, chapter_files);
    }
}
