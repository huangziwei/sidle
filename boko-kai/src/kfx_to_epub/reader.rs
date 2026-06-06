//! Reader-mode entry point: the KFX→DOM front half of `kfx_to_epub`, surfaced
//! for Sidle's built-in reader instead of being zipped into an EPUB.
//!
//! [`kfx_to_reader_book`] runs the *same* pipeline as
//! [`super::convert_to_epub`] — so the reader renders what the device actually
//! displayed — but with `data-eid` stamping on and the EPUB zip tail replaced
//! by a structured extraction of the spine documents, resources, and TOC.

use std::collections::HashSet;

use super::loader::{BookData, BookMetadata};
use super::navigation::NavPoint;
use super::text_index::TextIndex;
use super::{ConvertError, build_output};

/// One spine document in reading order. `html` is a complete XHTML string with
/// `data-eid` attributes on every addressable element (the reader resolves an
/// annotation's `(eid, offset)` by querying `[data-eid]` then walking text).
pub struct ReaderSection {
    pub href: String,
    pub html: String,
}

/// A non-spine asset the chapters reference by relative href (images,
/// `style.css`). The reader serves these into its render iframe.
pub struct ReaderResource {
    pub href: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// The reader's view of a book: rendered sections + the assets they reference +
/// navigation + metadata, all from the same pipeline that produces the EPUB.
/// TOC entry `href`s point into `sections` (e.g. `"c5.xhtml#anchor-12"`).
pub struct ReaderBook {
    pub sections: Vec<ReaderSection>,
    pub resources: Vec<ReaderResource>,
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
}

/// Convert a KFX container to the reader's [`ReaderBook`] — the KFX→DOM front
/// half with `data-eid` stamping, minus the EPUB zip.
pub fn kfx_to_reader_book(kfx_bytes: &[u8]) -> Result<ReaderBook, ConvertError> {
    let (out, book, toc) = build_output(kfx_bytes, true)?;
    // Drop the synthetic `titlepage.xhtml` cover wrapper that `build_output`
    // prepends for the EPUB export (Apple Books et al. need an explicit cover
    // spine item; see `build_titlepage`). The reader renders the KFX's own
    // spine, whose first section already IS the cover image (the KFX
    // `CoverPage`, e.g. `c0.xhtml` with the cover `<img>`) — exactly what the
    // device shows. Keeping the titlepage too would double the cover: a leading
    // page with no eid/Location, then the real cover section at Location 0.
    let sections: Vec<ReaderSection> = out
        .spine_documents()
        .into_iter()
        .filter(|(href, _)| href != "titlepage.xhtml")
        .map(|(href, html)| ReaderSection { href, html })
        .collect();
    let resources = out
        .non_spine_resources()
        .into_iter()
        .map(|(href, mime, data)| ReaderResource { href, mime, data })
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
    Ok(ReaderBook {
        sections,
        resources,
        toc,
        metadata: book.metadata,
        writing_mode,
        page_progression_direction,
        locations,
        max_location,
    })
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
        let bytes = std::fs::read(fixture("[太宰 治] 人間失格.kfx")).expect("read [太宰 治] 人間失格.kfx fixture");
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

    #[test]
    fn epub_export_does_not_stamp_data_eid() {
        let bytes = std::fs::read(fixture("[太宰 治] 人間失格.kfx")).expect("read [太宰 治] 人間失格.kfx fixture");
        // The shippable EPUB path must leave the DOM stamp-free (no bloat).
        let (out, _book, _toc) = build_output(&bytes, false).expect("build_output");
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
