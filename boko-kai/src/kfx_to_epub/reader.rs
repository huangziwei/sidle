//! Reader-mode entry point: the KFX→DOM front half of `kfx_to_epub`, surfaced
//! for Sidle's built-in reader instead of being zipped into an EPUB.
//!
//! [`kfx_to_reader_book`] runs the *same* pipeline as
//! [`super::convert_to_epub`] — so the reader renders what the device actually
//! displayed — but with `data-eid` stamping on and the EPUB zip tail replaced
//! by a structured extraction of the spine documents, resources, and TOC.

use std::collections::{HashMap, HashSet};

use super::loader::{BookData, BookMetadata};
use super::navigation::NavPoint;
use super::resources::{self, DeferredImage};
use super::text_index::TextIndex;
use super::{ConvertError, build_output};

/// One spine document in reading order. `html` is a complete XHTML string with
/// `data-eid` attributes on every addressable element (the reader resolves an
/// annotation's `(eid, offset)` by querying `[data-eid]` then walking text).
pub struct ReaderSection {
    pub href: String,
    pub html: String,
    /// Fixed-layout page pixel size, parsed from the document's
    /// `<meta name="viewport">`; `None` for reflowable documents. The reader
    /// uses it to size the page box before scaling to the screen.
    pub viewport: Option<(u32, u32)>,
    /// `"page-spread-left"` / `"page-spread-right"` for a paired fixed-layout
    /// page, else `None`. Drives two-up spread placement in the reader.
    pub spread: Option<String>,
}

/// A non-spine asset the chapters reference by relative href (images,
/// `style.css`). The reader serves these into its render iframe.
pub struct ReaderResource {
    pub href: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// Manifest entry for an image whose bytes are fetched on demand from the
/// [`ReaderImageStore`] — href/mime/dimensions only, so the open payload
/// stays small and the reader can reserve layout space before the pixels
/// arrive. `mime` is the predicted type; the fetch returns the actual one.
pub struct ReaderImage {
    pub href: String,
    pub mime: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// On-demand image supplier for a lazily-opened book: owns the parsed
/// [`BookData`] (source of the raw JXR/JPEG bytes) plus the deferred work
/// list keyed by href. Fetches transcode at request time — the expensive
/// JXR→JPEG work happens per image, when (and only when) the reader wants
/// it. Thread-safe by immutability: `fetch`/`fetch_many` take `&self`.
pub struct ReaderImageStore {
    book: BookData,
    items: HashMap<String, DeferredImage>,
}

impl ReaderImageStore {
    /// Produce one image's bytes: `(actual_mime, data)`. `None` for an href
    /// this book doesn't defer (already-fetched hrefs stay valid — the store
    /// is stateless, a re-fetch just re-transcodes).
    pub fn fetch(&self, href: &str) -> Option<Result<(String, Vec<u8>), ConvertError>> {
        let item = self.items.get(href)?;
        Some(resources::transcode_deferred_one(&self.book, item).map(|t| (t.mime, t.bytes)))
    }

    /// Fetch a batch **in parallel** (scoped threads, one slice per core —
    /// same policy as the EPUB export's transcode). Unknown hrefs are
    /// dropped from the result; per-item failures are returned so the caller
    /// can log/skip without failing the batch.
    #[allow(clippy::type_complexity)]
    pub fn fetch_many(
        &self,
        hrefs: &[String],
    ) -> Vec<(String, Result<(String, Vec<u8>), ConvertError>)> {
        let items: Vec<&DeferredImage> =
            hrefs.iter().filter_map(|h| self.items.get(h)).collect();
        let owned: Vec<DeferredImage> = items.iter().map(|d| (*d).clone()).collect();
        resources::transcode_deferred(&self.book, &owned)
            .into_iter()
            .zip(owned.iter())
            .map(|(r, d)| (d.filename.clone(), r.map(|t| (t.mime, t.bytes))))
            .collect()
    }

    /// Number of deferred images (the "total" behind a loaded-% indicator).
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when the book defers no images at all.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// The reader's view of a book: rendered sections + the assets they reference +
/// navigation + metadata, all from the same pipeline that produces the EPUB.
/// TOC entry `href`s point into `sections` (e.g. `"c5.xhtml#anchor-12"`).
pub struct ReaderBook {
    pub sections: Vec<ReaderSection>,
    /// Eagerly-shipped non-spine assets (`style.css`). In the lazy shape,
    /// images are NOT here — see `images`; the legacy [`kfx_to_reader_book`]
    /// inlines them here instead.
    pub resources: Vec<ReaderResource>,
    /// Deferred-image manifest (lazy shape only; empty in the legacy shape).
    /// Bytes come from the paired [`ReaderImageStore`] on demand.
    pub images: Vec<ReaderImage>,
    pub toc: Vec<NavPoint>,
    pub metadata: BookMetadata,
    /// Book-level writing mode, e.g. `"vertical-rl"` / `"horizontal-tb"`.
    pub writing_mode: String,
    /// Spine progression, e.g. `"rtl"` / `"ltr"`.
    pub page_progression_direction: String,
    /// `(eid, linear_position)` for positioned elements — the reader's "Location"
    /// readout. From `position_id_map` ($265) when present (Amazon KFX → the
    /// device's own Loc numbers); otherwise synthesized from reading order +
    /// base-text char counts (boko-generated e2k KFX ships no map). The
    /// `(eid, offset)` anchors that annotations and last-read use are intrinsic
    /// and independent of this — it's only the cosmetic Loc/% display.
    pub locations: Vec<(i64, i64)>,
    /// Largest linear position — the denominator for whole-book %.
    pub max_location: i64,
    /// Image-based fixed-layout book (manga / comic): the reader switches to
    /// pre-paginated rendering (one page per section, viewport-sized, two-up
    /// spreads) instead of reflowing text.
    pub fixed_layout: bool,
}

/// Convert a KFX container to the reader's [`ReaderBook`] with every image
/// inlined in `resources` — the KFX→DOM front half with `data-eid` stamping,
/// minus the EPUB zip. This is the legacy eager shape (tests, offline
/// harnesses); Sidle opens books via [`kfx_to_reader_book_lazy`] and streams
/// the images afterwards.
pub fn kfx_to_reader_book(kfx_bytes: &[u8]) -> Result<ReaderBook, ConvertError> {
    let (mut book, store) = kfx_to_reader_book_lazy(kfx_bytes)?;
    let hrefs: Vec<String> = book.images.iter().map(|i| i.href.clone()).collect();
    for (href, result) in store.fetch_many(&hrefs) {
        let (mime, data) = result?;
        book.resources.push(ReaderResource { href, mime, data });
    }
    book.images.clear();
    Ok(book)
}

/// Convert a KFX container to the reader's [`ReaderBook`] with image bytes
/// deferred: the book carries an image *manifest* (`images`) and the returned
/// [`ReaderImageStore`] produces each image on request. Opening a 100MB+
/// manga costs milliseconds of structure work instead of the full-book
/// JXR→JPEG transcode; the reader fetches the pages around the reading
/// position first and the rest in the background.
pub fn kfx_to_reader_book_lazy(
    kfx_bytes: &[u8],
) -> Result<(ReaderBook, ReaderImageStore), ConvertError> {
    let (out, book, toc, deferred) = build_output(kfx_bytes, true, &|_, _, _, _| {})?;
    // Drop the synthetic `titlepage.xhtml` cover wrapper that `build_output`
    // prepends for the EPUB export (Apple Books et al. need an explicit cover
    // spine item; see `build_titlepage`). The reader renders the KFX's own
    // spine, whose first section already IS the cover image (the KFX
    // `CoverPage`, e.g. `c0.xhtml` with the cover `<img>`) — exactly what the
    // device shows. Keeping the titlepage too would double the cover: a leading
    // page with no eid/Location, then the real cover section at Location 0.
    let fixed_layout = out.is_fixed_layout();
    let sections: Vec<ReaderSection> = out
        .spine_documents_with_props()
        .into_iter()
        .filter(|(href, _, _)| href != "titlepage.xhtml")
        .map(|(href, html, spread)| {
            let viewport = parse_viewport(&html);
            ReaderSection {
                href,
                html,
                viewport,
                spread,
            }
        })
        .collect();

    // Prepend the cover (表紙 / "Cover") as the leading TOC entry. Amazon's KFX
    // carries it, but boko's e2k KFX deliberately must NOT bake it into the
    // `book_navigation`: a TOC target landing inside the image-only cover
    // page-template wedges the strict Kindle firmware's front-matter pagination
    // (the device drops the entry AND stalls e-ink refresh until the first real
    // storyline position). So synthesize it here, reader-side only, pointing at
    // the cover section — the spine doc that shows the cover image. Skipped when
    // the book has no cover, that section isn't present, or the TOC already
    // leads with it (e.g. a real Amazon KFX, whose `book_navigation` has it).
    let mut toc = toc;
    // Compare by *section*, ignoring any `#fragment`: a real book_navigation
    // cover entry now carries a synthesized `#toc-…` anchor (see
    // `register_toc_anchors`), so an exact-href compare would miss the match and
    // prepend a duplicate 表紙.
    let leads_with_cover = |toc: &[NavPoint], sec: &str| {
        toc.first()
            .map(|p| p.href.split('#').next().unwrap_or(&p.href) == sec)
            .unwrap_or(false)
    };
    if let Some((cover_img, _, _)) = out.cover_image_info()
        && let Some(cover_sec) = sections.iter().find(|s| s.html.contains(cover_img))
        && !leads_with_cover(&toc, &cover_sec.href)
    {
        let label = if book
            .metadata
            .language
            .to_ascii_lowercase()
            .starts_with("ja")
        {
            "表紙"
        } else {
            "Cover"
        };
        toc.insert(
            0,
            NavPoint {
                label: label.to_string(),
                href: cover_sec.href.clone(),
                children: Vec::new(),
            },
        );
    }

    // Deferred images ship as a manifest; everything else non-spine with real
    // bytes (`style.css`) ships eagerly — the text render needs it up front.
    let deferred_hrefs: HashSet<&str> = deferred.iter().map(|d| d.filename.as_str()).collect();
    let resources = out
        .non_spine_resources()
        .into_iter()
        .filter(|(href, _, _)| !deferred_hrefs.contains(href.as_str()))
        .map(|(href, mime, data)| ReaderResource { href, mime, data })
        .collect();
    let images = deferred
        .iter()
        .map(|d| ReaderImage {
            href: d.filename.clone(),
            mime: d.mime.clone(),
            width: d.width,
            height: d.height,
        })
        .collect();
    let writing_mode = out
        .writing_mode
        .clone()
        .unwrap_or_else(|| "horizontal-tb".to_string());
    let page_progression_direction = out
        .page_progression_direction
        .clone()
        .unwrap_or_else(|| "ltr".to_string());
    let (locations, max_location) = compute_locations(&book, &sections);
    let metadata = book.metadata.clone();
    let store = ReaderImageStore {
        book,
        items: deferred
            .into_iter()
            .map(|d| (d.filename.clone(), d))
            .collect(),
    };
    Ok((
        ReaderBook {
            sections,
            resources,
            images,
            toc,
            metadata,
            writing_mode,
            page_progression_direction,
            locations,
            max_location,
            fixed_layout,
        },
        store,
    ))
}

/// Parse `width`/`height` from a `<meta name="viewport" content="width=W,
/// height=H">` tag (the only viewport form `content.rs` emits for fixed-layout
/// pages). Returns `None` when absent — i.e. a reflowable document.
fn parse_viewport(html: &str) -> Option<(u32, u32)> {
    let i = html.find("name=\"viewport\"")?;
    let content_start = html[i..].find("content=\"")? + i + "content=\"".len();
    let content_end = html[content_start..].find('"')? + content_start;
    let content = &html[content_start..content_end];
    let mut w = None;
    let mut h = None;
    for part in content.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("width=") {
            w = v.trim().parse::<u32>().ok();
        } else if let Some(v) = part.strip_prefix("height=") {
            h = v.trim().parse::<u32>().ok();
        }
    }
    match (w, h) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    }
}

/// Per-element linear positions for the reader's Location/% readout.
///
/// Uses the real `position_id_map` ($265) when the KFX has one (Amazon books →
/// the device's own Loc numbers). Otherwise synthesizes monotonic positions by
/// walking eids in **rendered reading order** (the sections are already in spine
/// order, with `data-eid` stamped in document order) and weighting each by its
/// base-text char count — because boko-generated e2k KFX ships no position map.
/// The synthesized numbers won't equal the device's (different unit), but the
/// whole-book % is faithful and the `(eid,offset)` anchors used by annotations /
/// last-read are unaffected (this is display-only).
fn compute_locations(book: &BookData, sections: &[ReaderSection]) -> (Vec<(i64, i64)>, i64) {
    let pid_of = TextIndex::pid_map_from_book(book);
    if !pid_of.is_empty() {
        let max = pid_of.values().copied().max().unwrap_or(0);
        return (pid_of.into_iter().collect(), max);
    }
    let index = TextIndex::from_book(book);
    let mut locations = Vec::new();
    let mut seen = HashSet::new();
    let mut pos: i64 = 0;
    for s in sections {
        for eid in scan_eids_in_order(&s.html) {
            if seen.insert(eid) {
                locations.push((eid, pos));
                pos += index.text_of(eid).map_or(0, |t| t.chars().count() as i64);
            }
        }
    }
    (locations, pos)
}

/// Eids in document order from a section's `data-eid="N"` stamps (reader mode
/// stamps these). A plain substring scan — no regex dep, and the stamp format
/// is fixed by `content.rs`.
fn scan_eids_in_order(html: &str) -> Vec<i64> {
    const NEEDLE: &str = "data-eid=\"";
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find(NEEDLE) {
        rest = &rest[i + NEEDLE.len()..];
        let end = rest.find('"').unwrap_or(rest.len());
        if let Ok(eid) = rest[..end].parse::<i64>() {
            out.push(eid);
        }
        rest = &rest[end..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn count_data_eid(book: &ReaderBook) -> usize {
        book.sections
            .iter()
            .map(|s| s.html.matches("data-eid=").count())
            .sum()
    }

    #[test]
    fn reader_book_has_sections_and_stamps_data_eid() {
        let bytes = std::fs::read(fixture("[太宰 治] 人間失格.kfx"))
            .expect("read [太宰 治] 人間失格.kfx fixture");
        let book = kfx_to_reader_book(&bytes).expect("kfx_to_reader_book");
        assert!(!book.sections.is_empty(), "expected at least one section");
        assert!(
            count_data_eid(&book) > 0,
            "reader mode must stamp data-eid; found none"
        );
        // The chapters reference `style.css`, so it must be a non-spine resource.
        assert!(
            book.resources.iter().any(|r| r.href == "style.css"),
            "expected style.css among reader resources"
        );
    }

    /// The reader surfaces the cover as the leading TOC entry even when the
    /// KFX's own `book_navigation` omits it (the fixture's toc starts at
    /// はしがき). boko's e2k KFX must NOT bake the cover into the toc — a TOC
    /// target inside the image-only cover page-template wedges the Kindle
    /// firmware — so it's synthesized reader-side, pointing at the cover
    /// section, and never doubled.
    #[test]
    fn reader_toc_leads_with_synthesized_cover() {
        let bytes = std::fs::read(fixture("[太宰 治] 人間失格.kfx")).expect("read fixture");
        let book = kfx_to_reader_book(&bytes).expect("kfx_to_reader_book");
        let first = book.toc.first().expect("toc should have entries");
        // 表紙 for this Japanese book, at the first rendered section (the cover,
        // the synthetic titlepage having been dropped).
        assert_eq!(first.label, "表紙");
        assert_eq!(first.href, book.sections[0].href);
        // Exactly once — synthesis must not double a cover a KFX already lists.
        assert_eq!(
            book.toc.iter().filter(|p| p.label == "表紙").count(),
            1,
            "cover TOC entry should appear exactly once"
        );
    }

    /// Lazy shape ↔ eager shape equivalence: every manifest entry fetches to
    /// the same bytes the legacy inline path ships, and nothing is lost or
    /// duplicated between `resources` and `images`.
    #[test]
    fn lazy_reader_book_matches_eager() {
        let bytes = std::fs::read(fixture("[太宰 治] 人間失格.kfx")).expect("read fixture");
        let eager = kfx_to_reader_book(&bytes).expect("eager");
        let (lazy, store) = kfx_to_reader_book_lazy(&bytes).expect("lazy");
        assert!(eager.images.is_empty(), "legacy shape inlines images");
        assert_eq!(store.len(), lazy.images.len());
        // Every eager resource is either an eager lazy resource (style.css) or
        // a deferred image whose fetched bytes match exactly.
        for r in &eager.resources {
            if let Some(img) = lazy.images.iter().find(|i| i.href == r.href) {
                let (mime, data) = store
                    .fetch(&img.href)
                    .expect("manifest href must fetch")
                    .expect("fetch must succeed");
                assert_eq!(data, r.data, "bytes differ for {}", r.href);
                assert_eq!(mime, r.mime, "mime differs for {}", r.href);
            } else {
                let e = lazy
                    .resources
                    .iter()
                    .find(|l| l.href == r.href)
                    .unwrap_or_else(|| panic!("{} missing from lazy shape", r.href));
                assert_eq!(e.data, r.data);
            }
        }
        assert_eq!(
            eager.resources.len(),
            lazy.resources.len() + lazy.images.len(),
            "no resource may be dropped or invented by the lazy split"
        );
    }

    #[test]
    fn parse_viewport_reads_fixed_layout_meta() {
        let html = r#"<html><head><meta name="viewport" content="width=900, height=1280"/></head><body><img src="p.jpg"/></body></html>"#;
        assert_eq!(parse_viewport(html), Some((900, 1280)));
        // Reflowable page (no viewport) → None.
        let reflow = r#"<html><head><title>c0</title></head><body><p>text</p></body></html>"#;
        assert_eq!(parse_viewport(reflow), None);
    }

    #[test]
    fn epub_export_does_not_stamp_data_eid() {
        let bytes = std::fs::read(fixture("[太宰 治] 人間失格.kfx"))
            .expect("read [太宰 治] 人間失格.kfx fixture");
        // The shippable EPUB path must leave the DOM stamp-free (no bloat).
        let (out, _book, _toc, _deferred) =
            build_output(&bytes, false, &|_, _, _, _| {}).expect("build_output");
        let stamped: usize = out
            .spine_documents()
            .iter()
            .map(|(_, html)| html.matches("data-eid=").count())
            .sum();
        assert_eq!(stamped, 0, "EPUB export must not stamp data-eid");
        // And the real EPUB still builds.
        assert!(
            !super::super::convert_to_epub(&bytes)
                .expect("convert_to_epub")
                .is_empty()
        );
    }
}
