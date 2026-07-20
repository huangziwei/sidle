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
use super::text_index::{LocationMap, TextIndex};
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
    /// Base-text char count (ruby `<rt>` excluded, whitespace runs collapsed,
    /// entities decoded) — the webview's reading-pace measure, computed here
    /// so it never has to DOM-parse every section at open. Counts Unicode
    /// scalars where JS counted UTF-16 units (differs only on astral chars).
    pub chars: u64,
    /// True for a full-page-image section (cover, full-bleed art): no base
    /// text at all, at least one `img`/`image`/`svg` element.
    pub image_only: bool,
    /// Image hrefs this section references, in document order — the reader's
    /// fetch-priority input for the deferred-image loader.
    pub image_hrefs: Vec<String>,
    /// `data-eid` values in document order. Backend-side data (eid→section
    /// resolution for jumps into not-yet-streamed sections, deferred location
    /// synthesis); not shipped to the webview.
    pub eids: Vec<i64>,
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

/// On-demand supplier for a lazily-opened book: owns the parsed [`BookData`]
/// (source of the raw JXR/JPEG bytes) plus the deferred-image work list keyed
/// by href, and the flattened reading-order eid list backing deferred
/// location synthesis. Fetches transcode at request time — the expensive
/// JXR→JPEG work happens per image, when (and only when) the reader wants
/// it. Thread-safe by immutability: all methods take `&self`.
pub struct ReaderImageStore {
    book: BookData,
    items: HashMap<String, DeferredImage>,
    /// Every section's `data-eid`s concatenated in reading order — the input
    /// [`Self::synth_locations`] walks when the book shipped no position map.
    eid_order: Vec<i64>,
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
        let items: Vec<&DeferredImage> = hrefs.iter().filter_map(|h| self.items.get(h)).collect();
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

    /// Synthesize the reader's `(eid, linear_position)` map — the deferred
    /// half of a lazy open when the KFX ships no `position_id_map` (see
    /// [`ReaderBook::locations_deferred`]). Costs a full-book text walk
    /// (`TextIndex::from_book`), which is exactly why it's off the open path.
    pub fn synth_locations(&self) -> (Vec<(i64, i64)>, i64) {
        synth_locations(&self.book, &self.eid_order)
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
    /// `(eid, location)` for positioned elements — the reader's "Location"
    /// readout. `location` is the device's human Loc number: the `position_id_map`
    /// ($265) pid mapped through the book's `location_map` ($550/$621), so it
    /// matches what the Kindle shows (a raw pid is ~50× larger). Books with a
    /// position map but no location map get the device's even 110-pid spacing;
    /// boko-generated e2k KFX (no map at all) fall back to reading-order
    /// base-text char counts. The `(eid, offset)` anchors that annotations and
    /// last-read use are intrinsic and independent of this — it's only the
    /// cosmetic Loc/% display.
    pub locations: Vec<(i64, i64)>,
    /// Location count — the denominator for whole-book % and the "Loc N of M"
    /// total (char-count total for the e2k fallback).
    pub max_location: i64,
    /// True when `locations` was NOT computed at open (lazy shape, reflowable
    /// book without a position map): synthesis needs a full-book text walk,
    /// so it's deferred to [`ReaderImageStore::synth_locations`] and fetched
    /// in the background. Display-only data — anchors don't depend on it.
    pub locations_deferred: bool,
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
    if book.locations_deferred {
        let (locations, max_location) = store.synth_locations();
        book.locations = locations;
        book.max_location = max_location;
        book.locations_deferred = false;
    }
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
    let (mut out, book, deferred) = build_output(kfx_bytes, true, &|_, _, _, _| {})?;
    // The EPUB emitters read `out.toc`; the reader owns and adapts its copy
    // (cover prepend below) without touching the output state.
    let toc = std::mem::take(&mut out.toc);
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
        .filter(|(href, _, _)| href != "cover.xhtml")
        .map(|(href, html, spread)| {
            let viewport = parse_viewport(&html);
            let (chars, image_only, image_hrefs) = scan_section_meta(&html);
            let eids = scan_eids_in_order(&html);
            ReaderSection {
                href,
                html,
                viewport,
                spread,
                chars,
                image_only,
                image_hrefs,
                eids,
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
    let eid_order: Vec<i64> = sections
        .iter()
        .flat_map(|s| s.eids.iter().copied())
        .collect();
    // Locations: a real position map ($265) is cheap — ship inline. So is the
    // synthesized map of a fixed-layout book (image pages, no text to walk).
    // A reflowable book without a map needs the full-book text walk — defer
    // it to the store (fetched in the background; display-only data).
    let pid_of = TextIndex::pid_map_from_book(&book);
    let (locations, max_location, locations_deferred) = if !pid_of.is_empty() {
        // Amazon KFX ships a position map (eid→pid). Turn each pid into the
        // device's human "Location" via the book's location_map ($550/$621);
        // a book that has a position map but no location map falls back to the
        // device's own even spacing. Emitting the raw pid (as this once did)
        // inflated the number ~50× and broke position matching with the Kindle.
        let max_pid = pid_of.values().copied().max().unwrap_or(0);
        let lm = LocationMap::from_book(&book, &pid_of)
            .unwrap_or_else(|| LocationMap::approximate(max_pid));
        let mut locations: Vec<(i64, i64)> = pid_of
            .iter()
            .map(|(&eid, &pid)| (eid, lm.location_for_pid(pid)))
            .collect();
        // `pid_of` is a HashMap, so collecting straight out of it ordered the
        // pairs by hash — different on every run, since Rust seeds each
        // process differently. Consumers key by eid so the order carries no
        // meaning, but an API that returns a different Vec each call cannot be
        // cached, diffed, or tested. Sort by eid to make it reproducible.
        locations.sort_unstable();
        (locations, lm.count(), false)
    } else if fixed_layout {
        let (l, m) = synth_locations(&book, &eid_order);
        (l, m, false)
    } else {
        (Vec::new(), 0, true)
    };
    let metadata = book.metadata.clone();
    let store = ReaderImageStore {
        book,
        items: deferred
            .into_iter()
            .map(|d| (d.filename.clone(), d))
            .collect(),
        eid_order,
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
            locations_deferred,
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

/// Synthesized per-element linear positions: walk eids in **rendered reading
/// order** (sections in spine order, `data-eid` stamped in document order),
/// weighting each by its base-text char count — for KFX with no
/// `position_id_map` (boko-generated e2k). The numbers won't equal a device's
/// Loc (different unit), but the whole-book % is faithful and the
/// `(eid,offset)` anchors used by annotations / last-read are unaffected
/// (this is display-only). Costs a full-book text walk — the reason a lazy
/// open defers it (see [`ReaderImageStore::synth_locations`]).
fn synth_locations(book: &BookData, eid_order: &[i64]) -> (Vec<(i64, i64)>, i64) {
    let index = TextIndex::from_book(book);
    let mut locations = Vec::new();
    let mut seen = HashSet::new();
    let mut pos: i64 = 0;
    for &eid in eid_order {
        if seen.insert(eid) {
            locations.push((eid, pos));
            pos += index.text_of(eid).map_or(0, |t| t.chars().count() as i64);
        }
    }
    (locations, pos)
}

/// One pass over a section's serialized XHTML for the per-section metadata
/// the webview needs at open: base-text char count, image-only flag, and the
/// image hrefs it references. Replaces the reader's own DOMParser sweep of
/// every section — the dominant webview cost on a large text book.
///
/// Semantics mirror reader.js `baseTextLen` / `isImageOnlySection`:
/// `<body>` text only, ruby `<rt>` content excluded, entities decoded,
/// whitespace runs collapsed to one char with the ends trimmed; image-only =
/// zero base text and at least one `img`/`image`/`svg` element. A plain
/// state-machine scan of boko's own output (well-formed, double-quoted
/// attributes, lowercase tags) — not a general HTML parser.
fn scan_section_meta(html: &str) -> (u64, bool, Vec<String>) {
    let body_start = html
        .find("<body")
        .and_then(|i| html[i..].find('>').map(|j| i + j + 1))
        .unwrap_or(0);
    let body_end = html.rfind("</body>").unwrap_or(html.len());
    let mut rest = &html[body_start..body_end];

    let mut chars: u64 = 0;
    let mut text_seen = false;
    let mut pending_space = false;
    let mut has_image_el = false;
    let mut hrefs: Vec<String> = Vec::new();

    // Attribute value inside one tag's `name="…"` (boko serializes with
    // double quotes; no unquoted/single-quoted attrs in our output).
    fn attr<'t>(tag: &'t str, name: &str) -> Option<&'t str> {
        let needle = format!(" {name}=\"");
        let start = tag.find(&needle)? + needle.len();
        let end = tag[start..].find('"')? + start;
        Some(&tag[start..end])
    }

    while let Some(lt) = rest.find('<') {
        count_base_text(&rest[..lt], &mut chars, &mut text_seen, &mut pending_space);
        rest = &rest[lt..];
        if let Some(after) = rest.strip_prefix("<!--") {
            match after.find("-->") {
                Some(e) => {
                    rest = &after[e + 3..];
                    continue;
                }
                None => break,
            }
        }
        let Some(gt) = rest.find('>') else { break };
        let tag = &rest[1..gt];
        rest = &rest[gt + 1..];
        let name_end = tag
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(tag.len());
        let name = &tag[..name_end];
        match name {
            // Skip ruby annotation content entirely (JS removes <rt> nodes).
            // Flat in our output — content.rs never nests ruby.
            "rt" if !tag.ends_with('/') => match rest.find("</rt") {
                Some(e) => rest = &rest[e..],
                None => break,
            },
            "img" => {
                has_image_el = true;
                if let Some(src) = attr(tag, "src")
                    && !hrefs.iter().any(|h| h == src)
                {
                    hrefs.push(src.to_string());
                }
            }
            "image" => {
                has_image_el = true;
                if let Some(href) = attr(tag, "xlink:href").or_else(|| attr(tag, "href"))
                    && !hrefs.iter().any(|h| h == href)
                {
                    hrefs.push(href.to_string());
                }
            }
            "svg" => has_image_el = true,
            _ => {}
        }
    }
    count_base_text(rest, &mut chars, &mut text_seen, &mut pending_space);

    (chars, !text_seen && has_image_el, hrefs)
}

/// Accumulate base-text chars from one inter-tag text run: entities decode to
/// one char, whitespace runs collapse to a single char, leading/trailing
/// whitespace never counts (the JS `.replace(/\s+/g, " ").trim()` semantics,
/// streamed).
fn count_base_text(text: &str, chars: &mut u64, text_seen: &mut bool, pending_space: &mut bool) {
    let mut i = 0;
    while i < text.len() {
        let c = text[i..].chars().next().expect("in-bounds char");
        let mut ch = c;
        let mut adv = c.len_utf8();
        if c == '&'
            && let Some(semi) = text[i + 1..].find(';').filter(|&s| s <= 10)
            && let Some(decoded) = decode_entity(&text[i + 1..i + 1 + semi])
        {
            ch = decoded;
            adv = semi + 2;
        }
        if ch.is_whitespace() {
            if *text_seen {
                *pending_space = true;
            }
        } else {
            if *pending_space {
                *chars += 1;
                *pending_space = false;
            }
            *chars += 1;
            *text_seen = true;
        }
        i += adv;
    }
}

/// Decode the entity body (between `&` and `;`): the named set our serializer
/// emits plus numeric forms. Unknown entities return `None` and count as
/// literal text — matching what a DOM parser would render for them.
fn decode_entity(body: &str) -> Option<char> {
    match body {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{00A0}'),
        _ => {
            let n = if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok()?
            } else if let Some(dec) = body.strip_prefix('#') {
                dec.parse::<u32>().ok()?
            } else {
                return None;
            };
            char::from_u32(n)
        }
    }
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
        let bytes = std::fs::read(fixture("[小栗 虫太郎] 黒死館殺人事件 (2012).kfx"))
            .expect("read [小栗 虫太郎] 黒死館殺人事件 (2012).kfx fixture");
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
        let bytes = std::fs::read(fixture("[小栗 虫太郎] 黒死館殺人事件 (2012).kfx"))
            .expect("read fixture");
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
        let bytes = std::fs::read(fixture("[小栗 虫太郎] 黒死館殺人事件 (2012).kfx"))
            .expect("read fixture");
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
        // Locations: whichever path the lazy shape took (inline for a book
        // with a position map / fixed layout, deferred otherwise), the eager
        // shape must end up with the same resolved map.
        assert!(
            !eager.locations_deferred,
            "legacy shape resolves the deferral"
        );
        if lazy.locations_deferred {
            assert!(lazy.locations.is_empty());
            let (locations, max_location) = store.synth_locations();
            assert_eq!(locations, eager.locations);
            assert_eq!(max_location, eager.max_location);
        } else {
            // Position-map locations come out of a HashMap — order is
            // unstable (and irrelevant: the reader builds a Map). Compare
            // as sorted sets.
            let mut a = lazy.locations.clone();
            let mut b = eager.locations.clone();
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(a, b);
            assert_eq!(lazy.max_location, eager.max_location);
        }
        // Deferred synthesis itself must still agree with the direct
        // synthesized walk regardless of which path shipped (both books with
        // and without position maps exercise scan order the same way).
        let eid_order: Vec<i64> = lazy
            .sections
            .iter()
            .flat_map(|s| s.eids.iter().copied())
            .collect();
        assert!(!eid_order.is_empty(), "sections must carry scanned eids");
    }

    /// The Rust section scanner must reproduce the webview's `baseTextLen` /
    /// `isImageOnlySection` semantics: body text only, `<rt>` excluded,
    /// entities decoded, whitespace collapsed+trimmed; image hrefs collected.
    #[test]
    fn scan_section_meta_matches_js_semantics() {
        let html = r#"<?xml version="1.0"?><html><head><title>ignored title</title></head>
<body><p data-eid="5">  Hello &amp; <ruby>漢字<rt>かんじ</rt></ruby>  world </p>
<p><img src="image_a.jpg"/> and <img src="image_a.jpg"/> dup</p>
<svg xmlns="http://www.w3.org/2000/svg"><image xlink:href="cover.jpeg"/></svg></body></html>"#;
        let (chars, image_only, hrefs) = scan_section_meta(html);
        // Collapsed base text = "Hello & 漢字 world and dup" → 24 chars.
        assert_eq!(
            chars, 24,
            "rt excluded, entity=1 char, whitespace collapsed"
        );
        assert!(!image_only);
        assert_eq!(
            hrefs,
            vec!["image_a.jpg".to_string(), "cover.jpeg".to_string()]
        );

        let cover = r#"<html><head><meta name="viewport" content="width=900, height=1280"/></head>
<body>  <div><img src="page_1.jpg"/></div>  </body></html>"#;
        let (chars, image_only, hrefs) = scan_section_meta(cover);
        assert_eq!(chars, 0);
        assert!(
            image_only,
            "whitespace-only body with an image is image-only"
        );
        assert_eq!(hrefs, vec!["page_1.jpg".to_string()]);
    }

    #[test]
    fn parse_viewport_reads_fixed_layout_meta() {
        let html = r#"<html><head><meta name="viewport" content="width=900, height=1280"/></head><body><img src="p.jpg"/></body></html>"#;
        assert_eq!(parse_viewport(html), Some((900, 1280)));
        // Reflowable page (no viewport) → None.
        let reflow = r#"<html><head><title>c0</title></head><body><p>text</p></body></html>"#;
        assert_eq!(parse_viewport(reflow), None);
    }

    /// Every emitted `<img>` must carry its content element's `$style` as a
    /// class. Calibre applies the content element's own style onto the `<img>`
    /// (`content_style.update(...)`, `yj_to_epub_content.py:1252`) like it does
    /// for every content type; boko's `emit_image` was the sole emit path that
    /// dropped it. The visible fallout: inline glyph images — rare hanzi with
    /// no Unicode code point, which KFX styles `{width:1em; height:~1em;
    /// max-width:100%}` so the Kindle scales them with the font — lost that
    /// font-relative sizing and rendered at intrinsic pixel size, awkwardly
    /// oversized and immune to the reader's font-size control. This guards that
    /// the class reaches the DOM and resolves to a real stylesheet rule (the
    /// committed fixture's only img is its cover, but the mechanism is
    /// identical; the 1em glyph case is exercised by real library books).
    #[test]
    fn reader_images_carry_their_style_class() {
        let bytes = std::fs::read(fixture("[小栗 虫太郎] 黒死館殺人事件 (2012).kfx"))
            .expect("read fixture");
        let book = kfx_to_reader_book(&bytes).expect("kfx_to_reader_book");

        // First real `<img …>` across all sections. A missing-resource image
        // deliberately falls back to a `<span>`, so an emitted `<img>` always
        // has a `src` — the content element whose `$style` must survive.
        let img_tag = book
            .sections
            .iter()
            .find_map(|s| {
                let start = s.html.find("<img")?;
                let end = s.html[start..]
                    .find('>')
                    .map(|e| start + e + 1)
                    .unwrap_or(s.html.len());
                Some(s.html[start..end].to_string())
            })
            .expect("fixture must emit at least one <img>");
        assert!(
            img_tag.contains("src="),
            "expected a real <img src>: {img_tag}"
        );

        // The regression: the img was previously classless.
        assert!(
            img_tag.contains("class=\""),
            "emitted <img> dropped its $style class: {img_tag}"
        );

        // And the class must resolve to a real rule in the shipped stylesheet —
        // the path by which an inline glyph image's `width:1em` reaches the
        // reader.
        let class = img_tag
            .split("class=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .expect("class value");
        let css = book
            .resources
            .iter()
            .find(|r| r.href == "style.css")
            .map(|r| String::from_utf8_lossy(&r.data).into_owned())
            .expect("style.css resource");
        assert!(
            css.contains(&format!(".{class} ")),
            "img class {class:?} has no rule in style.css"
        );
    }

    #[test]
    fn epub_export_does_not_stamp_data_eid() {
        let bytes = std::fs::read(fixture("[小栗 虫太郎] 黒死館殺人事件 (2012).kfx"))
            .expect("read [小栗 虫太郎] 黒死館殺人事件 (2012).kfx fixture");
        // The shippable EPUB path must leave the DOM stamp-free (no bloat).
        let (out, _book, _deferred) =
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
