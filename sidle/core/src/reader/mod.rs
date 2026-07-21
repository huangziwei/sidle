//! The reader's view of a book.
//!
//! Sidle renders books itself rather than handing them to a foreign EPUB
//! reader, so it needs a book in a shape no container defines: documents in
//! reading order that a webview can inject, per-document facts a paginator
//! needs before it paints, images fetched on demand rather than up front, and
//! the element ids that let a stored `(element, offset)` handle become a DOM
//! range.
//!
//! Every one of those comes out of bokai's IR — [`ReaderBook::open`] is
//! assembly, not derivation. Nothing here re-parses markup bokai just
//! produced; where an earlier implementation scanned serialized XHTML for
//! character counts, image lists, and element ids, the IR now answers each
//! structurally ([`bokai::model::ChapterSummary`],
//! [`bokai::model::Chapter::source_elements`]).

mod images;

pub use images::ImageStore;

use std::collections::HashMap;

use bokai::export::{PackageOptions, build_package};
use bokai::model::{Book, Format};

/// One document in reading order, plus what a paginator needs to place it.
pub struct ReaderSection {
    /// Filename the TOC and internal links point at.
    pub href: String,
    /// Complete XHTML, carrying `data-eid` on every addressable element.
    pub html: String,
    /// Fixed-layout page pixel box; `None` for reflowable documents.
    pub viewport: Option<(u32, u32)>,
    /// `"page-spread-left"` / `"page-spread-right"` for a paired fixed-layout
    /// page, else `None`.
    pub spread: Option<String>,
    /// Base-text character count — the reading-pace measure. Ruby annotations
    /// excluded, whitespace runs counted once.
    pub chars: u64,
    /// A full-page image (cover, full-bleed art) rather than prose.
    pub image_only: bool,
    /// Image hrefs this document references, in document order — the fetch
    /// priority for [`ImageStore`].
    pub image_hrefs: Vec<String>,
    /// Source element ids in document order. Resolves "which section holds
    /// element N" for a jump into a not-yet-streamed section.
    pub elements: Vec<i64>,
}

/// An image the reader fetches when it needs it. Bytes come from
/// [`ImageStore`]; this is enough to reserve layout space before they arrive.
pub struct ReaderImage {
    pub href: String,
    /// Predicted media type — the fetch returns the actual one.
    pub mime: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// A non-spine asset shipped up front because the first paint needs it.
pub struct ReaderResource {
    pub href: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// One entry in the reader's table of contents.
pub struct ReaderTocEntry {
    pub label: String,
    /// Points into [`ReaderBook::sections`], e.g. `"c5.xhtml#anchor-12"`.
    pub href: String,
    pub children: Vec<ReaderTocEntry>,
}

/// A book prepared for rendering.
pub struct ReaderBook {
    pub sections: Vec<ReaderSection>,
    /// Eagerly shipped non-spine assets (the stylesheet). Images are *not*
    /// here — see [`Self::images`].
    pub resources: Vec<ReaderResource>,
    /// Image manifest; bytes come from the paired [`ImageStore`].
    pub images: Vec<ReaderImage>,
    pub toc: Vec<ReaderTocEntry>,
    pub title: String,
    pub authors: Vec<String>,
    pub language: String,
    /// e.g. `"vertical-rl"` / `"horizontal-tb"`.
    pub writing_mode: String,
    /// e.g. `"rtl"` / `"ltr"`.
    pub page_progression_direction: String,
    /// `(element, location)` for positioned elements — the "Location" readout.
    /// This is the device's human Loc number, not a raw position: the source's
    /// position map mapped through its location map. Cosmetic; the
    /// `(element, offset)` anchors annotations use are independent of it.
    pub locations: Vec<(i64, i64)>,
    /// Denominator for whole-book percentage and "Loc N of M".
    pub max_location: i64,
    /// True when `locations` could not be computed because the source ships no
    /// position map. Display-only data, so the reader opens without it.
    pub locations_missing: bool,
    /// Image-based fixed layout (manga / comic): pre-paginated rendering, one
    /// page per section, two-up spreads.
    pub fixed_layout: bool,
}

impl ReaderBook {
    /// Prepare a KFX book for rendering, deferring image bytes.
    ///
    /// Opening a large illustrated book costs structure work, not a full-book
    /// image decode — the returned [`ImageStore`] produces each image when the
    /// reader asks for it.
    pub fn open(kfx: &[u8]) -> Result<(Self, ImageStore), String> {
        let mut book =
            Book::from_bytes(kfx, Format::Kfx).map_err(|e| format!("could not read KFX: {e}"))?;

        let positions = book.position_map().unwrap_or_default();
        let package = build_package(&mut book, PackageOptions::rendered(), &|_, _, _, _| {})
            .map_err(|e| format!("could not build the book: {e}"))?;

        // Per-document facts the paginator needs, read off the IR rather than
        // scanned back out of the XHTML above.
        let spine: Vec<_> = book.spine().iter().map(|s| s.id).collect();
        let mut summaries = Vec::with_capacity(spine.len());
        for id in spine {
            let chapter = book
                .load_chapter_cached(id)
                .map_err(|e| format!("could not load a chapter: {e}"))?;
            summaries.push((chapter.summary(), chapter.source_elements()));
        }

        // `zip` below would silently truncate to the shorter side, dropping
        // the tail of the book rather than reporting it.
        if package.documents.len() != summaries.len() {
            return Err(format!(
                "built {} documents for {} chapters",
                package.documents.len(),
                summaries.len()
            ));
        }
        // Every document the source produced is rendered, including the cover
        // page a container would drop (`EpubPackage::redundant_cover`). That
        // page carries the source elements; the synthesized SVG titlepage a
        // container ships in its place carries none, so it has no reading
        // position — rendering it would put a coverless, unaddressable page
        // ahead of the real cover at location 0. The titlepage is a separate
        // field and simply goes unused here.
        let sections: Vec<ReaderSection> = package
            .documents
            .into_iter()
            .zip(summaries)
            .map(|(doc, (summary, elements))| ReaderSection {
                href: doc.href,
                html: doc.xhtml,
                viewport: doc.viewport,
                spread: doc.spread,
                chars: summary.text_chars,
                image_only: summary.image_only,
                image_hrefs: summary.images,
                elements,
            })
            .collect();

        let images = package
            .assets
            .iter()
            .filter(|a| a.media_type.starts_with("image/"))
            .map(|a| ReaderImage {
                href: a.href.clone(),
                mime: a.media_type.clone(),
                width: a.width,
                height: a.height,
            })
            .collect();

        let mut resources = Vec::new();
        if !package.css.is_empty() {
            resources.push(ReaderResource {
                href: "style.css".to_string(),
                mime: "text/css".to_string(),
                data: package.css.into_bytes(),
            });
        }

        let writing_mode = package.writing_mode.clone();
        let (locations, max_location, locations_missing) = if positions.is_empty() {
            (Vec::new(), 0, true)
        } else {
            (
                positions.element_locations(),
                positions.location_count(),
                false,
            )
        };

        let metadata = book.metadata();
        let reader = ReaderBook {
            sections,
            resources,
            images,
            toc: map_toc(package.toc),
            title: metadata.title.clone(),
            authors: metadata.authors.clone(),
            language: metadata.language.clone(),
            writing_mode,
            page_progression_direction: metadata
                .page_progression_direction
                .clone()
                .unwrap_or_else(|| "ltr".to_string()),
            locations,
            max_location,
            locations_missing,
            fixed_layout: metadata.fixed_layout,
        };
        Ok((reader, ImageStore::new(book)))
    }

    /// Which section holds a given source element, for a jump into a section
    /// the reader has not streamed yet.
    pub fn section_of_element(&self) -> HashMap<i64, usize> {
        let mut out = HashMap::new();
        for (i, section) in self.sections.iter().enumerate() {
            for &e in &section.elements {
                out.entry(e).or_insert(i);
            }
        }
        out
    }
}

fn map_toc(points: Vec<bokai::export::nav::NavPoint>) -> Vec<ReaderTocEntry> {
    points
        .into_iter()
        .map(|p| ReaderTocEntry {
            label: p.label,
            href: p.href,
            children: map_toc(p.children),
        })
        .collect()
}
