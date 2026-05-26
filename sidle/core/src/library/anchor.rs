//! Resolve `.yjr` annotation anchors against a book's KFX content.
//!
//! Each [`yjr::Annotation`](super::yjr::Annotation) carries `(eid, offset)`
//! handles into KFX elements; this module turns those handles into the *text*
//! the annotation covers plus its linear positions, using boko's
//! [`TextIndex`](boko::kfx_to_epub::TextIndex) (the eid→text entry point).
//!
//! Division of labour:
//!   - `yjr.rs` decodes the device file into handles + note bodies;
//!   - `boko::kfx_to_epub::TextIndex` maps `eid → base text` + reading order;
//!   - **this module** joins them into a [`Resolved`] record;
//!   - `ingest.rs` folds in book identity + a dedup hash and writes the DB.
//!
//! Kindle handle semantics (locked in P0): the end offset is **inclusive**, so
//! a range extraction passes `off_end + 1` to `TextIndex::extract`, which is
//! half-open. A bookmark anchors a point, not a span, so its "text" is the
//! containing element's text — a location preview for the bookmark list.

use boko::kfx_to_epub::TextIndex;

use super::yjr::{Annotation, Kind};

/// An annotation resolved against its book's content: the covered text plus the
/// anchor/position fields the DB stores. Book identity, the dedup hash, and the
/// import timestamp are added by the ingest layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub kind: Kind,
    pub eid_start: Option<i64>,
    pub off_start: Option<i64>,
    pub eid_end: Option<i64>,
    pub off_end: Option<i64>,
    /// `pid(eid)+offset` for the start/end anchors, from the KFX position map —
    /// the same coordinate system as the extracted text.
    pub loc_start: Option<i64>,
    pub loc_end: Option<i64>,
    /// The device-reported linear position of the start anchor (Whispersync
    /// "Location"); authoritative for ordering.
    pub linear_pos: Option<i64>,
    /// The text the annotation covers. Empty when a highlight/note couldn't be
    /// resolved (its eid is absent from this KFX — a yjr/book mismatch); for a
    /// bookmark it's the containing-element preview.
    pub text: String,
    pub note_body: Option<String>,
}

impl Resolved {
    /// Whether the annotation resolved to any text. A highlight/note with no
    /// text failed to resolve (eid not in this book); a bookmark may legitimately
    /// be empty (its element carried no text).
    pub fn has_text(&self) -> bool {
        !self.text.is_empty()
    }
}

/// Resolve one annotation against the book's [`TextIndex`].
pub fn resolve(ann: &Annotation, idx: &TextIndex) -> Resolved {
    let start = ann.start();
    let end = ann.end();

    let eid_start = start.map(|h| h.eid as i64);
    let off_start = start.map(|h| h.offset as i64);
    let eid_end = end.map(|h| h.eid as i64);
    let off_end = end.map(|h| h.offset as i64);

    let loc_start = start.and_then(|h| idx.position(h.eid as i64, h.offset as i64));
    let loc_end = end.and_then(|h| idx.position(h.eid as i64, h.offset as i64));
    let linear_pos = start.map(|h| h.linear as i64);

    let text = match &ann.kind {
        // Span-bearing kinds: walk the range. The device end offset is
        // inclusive, so pass `off_end + 1` to the half-open extractor.
        Kind::Highlight | Kind::Note | Kind::Other(_) => match (start, end) {
            (Some(s), Some(e)) => idx
                .extract(
                    s.eid as i64,
                    s.offset as usize,
                    e.eid as i64,
                    e.offset as usize + 1,
                )
                .unwrap_or_default(),
            _ => String::new(),
        },
        // A point anchor: preview the containing element.
        Kind::Bookmark => start
            .and_then(|h| idx.text_of(h.eid as i64))
            .map(str::to_string)
            .unwrap_or_default(),
    };

    Resolved {
        kind: ann.kind.clone(),
        eid_start,
        off_start,
        eid_end,
        off_end,
        loc_start,
        loc_end,
        linear_pos,
        text,
        note_body: ann.note_body.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::yjr::Handle;
    use std::collections::HashMap;

    fn handle(eid: u32, offset: u32, linear: u64) -> Handle {
        Handle {
            type_byte: 1,
            eid,
            offset,
            linear,
            b64: String::new(),
        }
    }

    /// Three elements in pid order 30→20→10, mirroring the multi-element walk.
    fn index() -> TextIndex {
        let text_of: HashMap<i64, String> = [
            (30, "aaaBBB".to_string()),
            (20, "FULL".to_string()),
            (10, "GGGhhh".to_string()),
        ]
        .into_iter()
        .collect();
        let pid_of: HashMap<i64, i64> = [(30, 500), (20, 506), (10, 510)].into_iter().collect();
        TextIndex::from_parts(text_of, pid_of)
    }

    #[test]
    fn resolves_highlight_range_with_inclusive_end() {
        let idx = index();
        // Highlight from (30, 3) to (10, 2): off_end is inclusive, so element 10
        // contributes chars [0, 3) = "GGG" (2 inclusive → 3 exclusive).
        let ann = Annotation {
            kind: Kind::Highlight,
            handles: vec![handle(30, 3, 503), handle(10, 2, 512)],
            note_body: None,
        };
        let r = resolve(&ann, &idx);
        assert_eq!(r.text, "BBBFULLGGG");
        assert_eq!(r.eid_start, Some(30));
        assert_eq!(r.off_start, Some(3));
        assert_eq!(r.eid_end, Some(10));
        assert_eq!(r.off_end, Some(2));
        // loc = pid + offset; linear = device-reported.
        assert_eq!(r.loc_start, Some(503)); // 500 + 3
        assert_eq!(r.loc_end, Some(512)); // 510 + 2
        assert_eq!(r.linear_pos, Some(503));
        assert!(r.note_body.is_none());
        assert!(r.has_text());
    }

    #[test]
    fn resolves_note_with_inline_body() {
        let idx = index();
        let ann = Annotation {
            kind: Kind::Note,
            handles: vec![handle(20, 0, 506), handle(20, 3, 509)],
            note_body: Some("my thought".to_string()),
        };
        let r = resolve(&ann, &idx);
        // Single element, [0, 4) (3 inclusive → 4) → whole "FULL".
        assert_eq!(r.text, "FULL");
        assert_eq!(r.note_body.as_deref(), Some("my thought"));
    }

    #[test]
    fn resolves_bookmark_to_element_preview() {
        let idx = index();
        // Bookmarks repeat the start handle as the end.
        let ann = Annotation {
            kind: Kind::Bookmark,
            handles: vec![handle(20, 0, 506), handle(20, 0, 506)],
            note_body: None,
        };
        let r = resolve(&ann, &idx);
        // Preview = the whole containing element, not a 1-char slice.
        assert_eq!(r.text, "FULL");
        assert_eq!(r.eid_start, Some(20));
    }

    #[test]
    fn unresolvable_highlight_yields_empty_text() {
        let idx = index();
        let ann = Annotation {
            kind: Kind::Highlight,
            handles: vec![handle(999, 0, 0), handle(999, 5, 0)],
            note_body: None,
        };
        let r = resolve(&ann, &idx);
        assert_eq!(r.text, "");
        assert!(!r.has_text());
        // Anchor fields still carry the (unresolved) handle data.
        assert_eq!(r.eid_start, Some(999));
        assert_eq!(r.loc_start, None); // no pid for an unknown eid
    }
}
