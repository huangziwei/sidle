//! Resolve `.yjr` annotation anchors against a book's KFX content.
//!
//! Each [`yjr::Annotation`](super::yjr::Annotation) carries `(eid, offset)`
//! handles into KFX elements; this module turns those handles into the *text*
//! the annotation covers plus its linear positions, reading both through a
//! [`BookIndex`].
//!
//! Division of labour:
//!   - `yjr.rs` decodes the device file into handles + note bodies;
//!   - bokai supplies `eid → base text` and the position scale;
//!   - **this module** joins them into a [`Resolved`] record;
//!   - `ingest.rs` folds in book identity + a dedup hash and writes the DB.
//!
//! Kindle handle semantics (locked in P0): the end offset is **inclusive**, so
//! a range extraction passes `off_end + 1` to [`BookIndex::extract`], which is
//! half-open. A bookmark anchors a point, not a span, so its "text" is the
//! containing element's text — a location preview for the bookmark list.

use std::collections::HashMap;

use bokai::Book;
use bokai::model::{Format, PositionMap, SourceText};

use super::yjr::{Annotation, Kind};

/// A book's anchoring index: the base text the source stores per element, plus
/// the position scale those elements sit on.
///
/// The two travel together because resolving one handle draws on both — the
/// text says what characters an offset lands on, the scale says where in the
/// book it is — but they are independent: a book can carry positions and no
/// text at all. Building one is a full parse of the book's text, so callers
/// cache it (see the reader's per-session cache) rather than rebuilding per
/// query.
pub struct BookIndex {
    text: SourceText,
    positions: PositionMap,
}

impl BookIndex {
    /// Index a KFX from its bytes. `None` only when the container doesn't
    /// parse.
    ///
    /// The two halves are taken independently: an image-only fixed-layout book
    /// carries element positions and no base text at all. Requiring both would
    /// throw away its positions, and `loc_start` feeds `dedup_hash` — so every
    /// annotation in such a book would hash differently and re-import as a
    /// duplicate on the next sync.
    pub fn from_kfx(bytes: &[u8]) -> Option<Self> {
        let mut book = Book::from_bytes(bytes, Format::Kfx).ok()?;
        let positions = book.position_map().unwrap_or_default();
        let text = book
            .source_text()
            .unwrap_or_else(|| SourceText::new(HashMap::new(), &positions));
        Some(Self { text, positions })
    }

    /// Index from raw maps, for callers that already have them — unit tests,
    /// and any future reader that captures text during its own walk.
    pub fn from_parts(text_of: HashMap<i64, String>, pid_of: HashMap<i64, i64>) -> Self {
        let positions = PositionMap::new(pid_of, Vec::new());
        let text = SourceText::new(text_of, &positions);
        Self { text, positions }
    }

    /// An index over nothing — every lookup misses. Annotations still import
    /// with their anchors intact; only the text is unavailable.
    pub fn empty() -> Self {
        Self::from_parts(HashMap::new(), HashMap::new())
    }

    /// Whether no element carries text — an absent or unreadable KFX, or an
    /// image-only book. Positions may still resolve; check [`Self::position`].
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Base text of one element.
    pub fn text_of(&self, eid: i64) -> Option<&str> {
        self.text.text_of(eid)
    }

    /// Linear position of `(eid, offset)` on the book's scale.
    pub fn position(&self, eid: i64, offset: i64) -> Option<i64> {
        self.positions.position(eid, offset)
    }

    /// Text spanned by a half-open `[start, end)` range, in character indices.
    pub fn extract(
        &self,
        eid_start: i64,
        off_start: usize,
        eid_end: i64,
        off_end: usize,
    ) -> Option<String> {
        self.text.extract(eid_start, off_start, eid_end, off_end)
    }

    /// Every occurrence of `needle`, in reading order. v1 = strict char match,
    /// ASCII case-insensitive only — no NFKC or kata→hira folding (`「ＡＢＣ」`
    /// won't match `abc`; カタカナ won't match かたかな). v1 is also intra-element:
    /// a match must fit inside one element, which narrative text almost always
    /// does. Ruby `<rt>` text is not part of an element's base text, so it is
    /// skipped for free.
    ///
    /// Capped at [`MAX_RESULTS`]; each hit carries up to [`PREVIEW_CHARS`] of
    /// context per side, drawn from the *original* casing.
    pub fn search(&self, needle: &str) -> Vec<SearchMatch> {
        if needle.is_empty() {
            return Vec::new();
        }
        let needle_lower: Vec<char> = needle.chars().map(|c| c.to_ascii_lowercase()).collect();
        let nlen = needle_lower.len();
        let mut out = Vec::new();

        for &eid in self.text.reading_order() {
            if out.len() >= MAX_RESULTS {
                break;
            }
            let Some(text) = self.text.text_of(eid) else {
                continue;
            };
            let chars: Vec<char> = text.chars().collect();
            if chars.len() < nlen {
                continue;
            }
            let lower_chars: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
            let pid = self.positions.position(eid, 0).unwrap_or(0);
            // Non-overlapping scan: stepping by `nlen` after a hit avoids
            // double-reporting `aaa` inside `aaaaa`.
            let mut i = 0;
            while i + nlen <= lower_chars.len() {
                if lower_chars[i..i + nlen] == needle_lower[..] {
                    let before_start = i.saturating_sub(PREVIEW_CHARS);
                    let after_end = (i + nlen + PREVIEW_CHARS).min(chars.len());
                    out.push(SearchMatch {
                        eid,
                        off_start: i,
                        off_end: i + nlen - 1,
                        linear_pos: pid + i as i64,
                        preview_before: chars[before_start..i].iter().collect(),
                        preview_match: chars[i..i + nlen].iter().collect(),
                        preview_after: chars[i + nlen..after_end].iter().collect(),
                    });
                    if out.len() >= MAX_RESULTS {
                        break;
                    }
                    i += nlen;
                } else {
                    i += 1;
                }
            }
        }
        out
    }
}

/// Context characters kept on each side of a search hit.
const PREVIEW_CHARS: usize = 32;
/// Upper bound on hits returned from one search.
const MAX_RESULTS: usize = 1000;

/// One search hit, expressed as an annotation-shaped anchor plus preview text.
/// `off_end` is the **inclusive** last-char index, matching the
/// `(eid_start, off_start, eid_end, off_end)` convention the DB and the
/// webview's range walk both use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub eid: i64,
    pub off_start: usize,
    pub off_end: usize,
    pub linear_pos: i64,
    pub preview_before: String,
    pub preview_match: String,
    pub preview_after: String,
}

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

/// Resolve one annotation against the book's [`BookIndex`].
pub fn resolve(ann: &Annotation, idx: &BookIndex) -> Resolved {
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
        // Handwritten ink covers no text — it's routed to the ink path and never
        // reaches the text `annotations` table (import_yjr filters it out before
        // this), but the arm keeps the match exhaustive.
        Kind::Handwritten => String::new(),
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
    use super::super::yjr::Handle;
    use super::*;
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
    fn index() -> BookIndex {
        let text_of: HashMap<i64, String> = [
            (30, "aaaBBB".to_string()),
            (20, "FULL".to_string()),
            (10, "GGGhhh".to_string()),
        ]
        .into_iter()
        .collect();
        let pid_of: HashMap<i64, i64> = [(30, 500), (20, 506), (10, 510)].into_iter().collect();
        BookIndex::from_parts(text_of, pid_of)
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

    /// An image-only fixed-layout book carries element positions and no base
    /// text. Its positions must survive: `loc_start` feeds `dedup_hash`, so
    /// dropping them would make every annotation in such a book hash
    /// differently and re-import as a duplicate on the next device sync.
    #[test]
    fn positions_survive_a_book_with_no_base_text() {
        let idx =
            BookIndex::from_parts(HashMap::new(), [(30, 500), (20, 506)].into_iter().collect());
        assert!(idx.is_empty(), "no text to index");
        assert_eq!(idx.position(30, 4), Some(504), "positions still resolve");

        let ann = Annotation {
            kind: Kind::Highlight,
            handles: vec![handle(30, 4, 900), handle(20, 1, 906)],
            note_body: None,
        };
        let r = resolve(&ann, &idx);
        assert_eq!(r.text, "", "nothing to extract");
        assert_eq!(r.loc_start, Some(504), "but the anchor is still placed");
        assert_eq!(r.loc_end, Some(507));
    }
}
