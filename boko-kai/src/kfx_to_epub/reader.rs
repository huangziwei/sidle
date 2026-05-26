//! Reader-mode entry point: the KFX→DOM front half of `kfx_to_epub`, surfaced
//! for Sidle's built-in reader instead of being zipped into an EPUB.
//!
//! [`kfx_to_reader_book`] runs the *same* pipeline as
//! [`super::convert_to_epub`] — so the reader renders what the device actually
//! displayed — but with `data-eid` stamping on and the EPUB zip tail replaced
//! by a structured extraction of the spine documents, resources, and TOC.

use super::loader::BookMetadata;
use super::navigation::NavPoint;
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
}

/// Convert a KFX container to the reader's [`ReaderBook`] — the KFX→DOM front
/// half with `data-eid` stamping, minus the EPUB zip.
pub fn kfx_to_reader_book(kfx_bytes: &[u8]) -> Result<ReaderBook, ConvertError> {
    let (out, book, toc) = build_output(kfx_bytes, true)?;
    let sections = out
        .spine_documents()
        .into_iter()
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
    Ok(ReaderBook {
        sections,
        resources,
        toc,
        metadata: book.metadata,
        writing_mode,
        page_progression_direction,
    })
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
        let bytes = std::fs::read(fixture("epictetus.kfx")).expect("read epictetus.kfx fixture");
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
        let bytes = std::fs::read(fixture("epictetus.kfx")).expect("read epictetus.kfx fixture");
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

    #[test]
    fn real_corpus_bungaku_is_vertical_and_stamps_known_eid() {
        // 文学少女 — vertical-rl + ruby; the famous opening `<p>` is eid 968.
        // Gitignored corpus: skip with a message when absent (no committed data).
        let p0 = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("artifacts/p0");
        let bungaku = p0.join("bungaku.kfx");
        if !bungaku.exists() {
            eprintln!("skipping bungaku reader check: {bungaku:?} not present");
            return;
        }
        let bytes = std::fs::read(&bungaku).expect("read bungaku.kfx");
        let book = kfx_to_reader_book(&bytes).expect("kfx_to_reader_book");
        assert_eq!(book.writing_mode, "vertical-rl");
        assert!(
            book.sections.iter().any(|s| s.html.contains("data-eid=\"968\"")),
            "expected eid 968 (opening paragraph) stamped in some section"
        );
    }
}
