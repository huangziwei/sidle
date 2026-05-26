//! Resolve Kindle annotation anchors (`eid:offset`) against KFX content.
//!
//! A `.yjr` annotation handle names a content element by its `$155 id`
//! ("eid") plus a character offset into that element's base text. This is the
//! read-side counterpart to the EPUB export: it reuses the loader and the
//! `$145 content` resolution, but builds an eid → text index instead of a DOM.
//!
//! Ported from the P0 anchor proof (`artifacts/p0/extract.py`), which was
//! verified against `My Clippings.txt` on real device corpora:
//!   - `position_id_map` ($265): `eid → pid`, the reading-order sort key.
//!   - `storyline`        ($259): walked recursively; every struct with a
//!     `$155 id` and `$145 content` contributes `eid → resolve_content_text`.
//!   - `content`          ($145 frag): the strings, via `resolve_content_text`.
//!
//! Highlight text is a **per-element reading-order walk** between the start and
//! end handles: slice the first element from `off_start`, take middle elements
//! whole, slice the last to `off_end`. The walk is keyed on pid order (not
//! `by_type` iteration order, which is arbitrary, nor a naive pid *stream*,
//! which TOC/blurb PID inflation breaks).
//!
//! Offsets are **character** (Unicode scalar) indices — matching Python's
//! codepoint slicing in the proof — so the walk slices `chars()`, never bytes.
//! The walk is half-open `[off_start, off_end)`; Kindle's `.yjr` end offsets
//! are inclusive, so callers resolving those handles pass `off_end + 1`.

use std::collections::HashMap;

use crate::kfx::container::get_field;
use crate::kfx::ion::IonValue;
use crate::kfx::symbols::KfxSymbol;

use super::content::resolve_content_text;
use super::loader::{self, BookData};
use super::ConvertError;

/// eid → text + reading-order index for a single KFX container.
pub struct TextIndex {
    /// eid → base text of that element. Ruby annotation text is *not* here:
    /// `$145 content` resolves to the base run only; `<rt>` lives in a
    /// separate `ruby_content` fragment and never enters this map.
    text_of: HashMap<i64, String>,
    /// eid → pid (absolute linear position of the element's first character),
    /// from `position_id_map`.
    pid_of: HashMap<i64, i64>,
    /// eids that have both text and a pid, in reading order (sorted by pid).
    order: Vec<i64>,
    /// Inverse of `order`: eid → its rank (index into `order`).
    rank: HashMap<i64, usize>,
}

impl TextIndex {
    /// Load a KFX container and build the index.
    pub fn from_kfx(kfx_bytes: &[u8]) -> Result<Self, ConvertError> {
        let book = loader::load(kfx_bytes)?;
        Ok(Self::from_book(&book))
    }

    /// Build the index from an already-loaded container (so the reader can
    /// share one `load` between the DOM render and anchor resolution).
    pub fn from_book(book: &BookData) -> Self {
        let mut text_of = HashMap::new();
        if let Some(storylines) = book.by_type.get(&(KfxSymbol::Storyline as u64)) {
            for frag in storylines.values() {
                collect_eid_text(frag, book, &mut text_of);
            }
        }
        Self::index(text_of, Self::pid_map_from_book(book))
    }

    /// The `eid → pid` map from `position_id_map` ($265) alone, skipping the
    /// (more expensive) storyline text walk — for callers that need positions
    /// but not extracted text (e.g. the reader's Location readout). Empty when
    /// the KFX carries no position map (boko-generated e2k output).
    pub fn pid_map_from_book(book: &BookData) -> HashMap<i64, i64> {
        let mut pid_of = HashMap::new();
        if let Some(maps) = book.by_type.get(&(KfxSymbol::PositionIdMap as u64)) {
            for frag in maps.values() {
                let Some(entries) = frag.unwrap_annotated().as_list() else {
                    continue;
                };
                for entry in entries {
                    let Some(fields) = entry.unwrap_annotated().as_struct() else {
                        continue;
                    };
                    if let Some(eid) = get_field(fields, KfxSymbol::Eid as u64).and_then(|v| v.as_int())
                        && let Some(pid) = get_field(fields, KfxSymbol::Pid as u64).and_then(|v| v.as_int())
                    {
                        pid_of.insert(eid, pid);
                    }
                }
            }
        }
        pid_of
    }

    /// Build an index directly from `eid → text` and `eid → pid` maps, for
    /// callers that already have them: unit tests, and a future reader that
    /// extracts text during the DOM render and wants anchor resolution without
    /// re-deriving the maps.
    pub fn from_parts(text_of: HashMap<i64, String>, pid_of: HashMap<i64, i64>) -> Self {
        Self::index(text_of, pid_of)
    }

    /// Assemble `order`/`rank` from the two maps. Reading order = the eids
    /// that have both text and a pid, sorted by pid.
    fn index(text_of: HashMap<i64, String>, pid_of: HashMap<i64, i64>) -> Self {
        let mut order: Vec<i64> = text_of
            .keys()
            .copied()
            .filter(|e| pid_of.contains_key(e))
            .collect();
        order.sort_by_key(|e| pid_of[e]);
        let rank = order.iter().enumerate().map(|(i, &e)| (e, i)).collect();
        Self {
            text_of,
            pid_of,
            order,
            rank,
        }
    }

    /// Base text of a single element, if known.
    pub fn text_of(&self, eid: i64) -> Option<&str> {
        self.text_of.get(&eid).map(String::as_str)
    }

    /// Absolute linear position `pid(eid) + offset`, if the eid has a pid.
    pub fn position(&self, eid: i64, offset: i64) -> Option<i64> {
        self.pid_of.get(&eid).map(|p| p + offset)
    }

    /// Number of indexed (text-bearing, positioned) elements.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether the index resolved no positioned text elements at all.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Extract the text spanned by a `[start, end)` highlight: a per-element
    /// reading-order walk from `eid_start` to `eid_end` (inclusive of both
    /// elements), slicing the first element from `off_start` and bounding the
    /// last at `off_end`. Offsets are character indices.
    ///
    /// Returns `None` if either eid is unknown, or if `eid_end` precedes
    /// `eid_start` in reading order. Out-of-range offsets are clamped rather
    /// than panicking, so a malformed handle yields a best-effort substring.
    pub fn extract(
        &self,
        eid_start: i64,
        off_start: usize,
        eid_end: i64,
        off_end: usize,
    ) -> Option<String> {
        let &i = self.rank.get(&eid_start)?;
        let &j = self.rank.get(&eid_end)?;
        if j < i {
            return None;
        }
        let mut out = String::new();
        for k in i..=j {
            let eid = self.order[k];
            let chars: Vec<char> = self.text_of.get(&eid).map(|t| t.chars().collect()).unwrap_or_default();
            let a = if k == i { off_start } else { 0 };
            let b = if k == j { off_end } else { chars.len() };
            let a = a.min(chars.len());
            let b = b.min(chars.len()).max(a);
            out.extend(&chars[a..b]);
        }
        Some(out)
    }
}

/// Recursively walk a storyline fragment. Every struct that carries a
/// `$155 id` and a `$145 content` reference contributes `eid → base text`;
/// `$146 content_list` children are recursed into. `$176 story_name`
/// references are *not* followed — each referenced story is its own
/// `by_type[$259]` fragment and is walked at the top level instead.
fn collect_eid_text(value: &IonValue, book: &BookData, out: &mut HashMap<i64, String>) {
    let inner = value.unwrap_annotated();
    if let Some(fields) = inner.as_struct() {
        if let Some(eid) = get_field(fields, KfxSymbol::Id as u64).and_then(|v| v.as_int())
            && let Some(content) = get_field(fields, KfxSymbol::Content as u64)
        {
            let text = resolve_content_text(content, book);
            if !text.is_empty() {
                out.insert(eid, text);
            }
        }
        if let Some(list) = get_field(fields, KfxSymbol::ContentList as u64).and_then(|v| v.as_list()) {
            for child in list {
                collect_eid_text(child, book, out);
            }
        }
    } else if let Some(list) = inner.as_list() {
        for item in list {
            collect_eid_text(item, book, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(text: &[(i64, &str)], pids: &[(i64, i64)]) -> TextIndex {
        let text_of = text.iter().map(|(e, t)| (*e, t.to_string())).collect();
        let pid_of = pids.iter().copied().collect();
        TextIndex::index(text_of, pid_of)
    }

    #[test]
    fn extract_single_element_is_a_char_slice() {
        let t = idx(&[(10, "「夜の海。静寂の時。」")], &[(10, 100)]);
        // [1, 9) over the codepoints, skipping the opening 「 and stopping
        // before the closing 。 — must slice chars, not bytes.
        assert_eq!(t.extract(10, 1, 10, 9).as_deref(), Some("夜の海。静寂の時"));
    }

    #[test]
    fn extract_walks_multiple_elements_in_pid_order() {
        // pid order is 5,6,7 → eids 30,20,10 (deliberately not eid order, and
        // deliberately not HashMap-insertion order).
        let t = idx(
            &[(10, "GGGhhh"), (20, "FULL"), (30, "aaaBBB")],
            &[(30, 5), (20, 6), (10, 7)],
        );
        // First element (eid 30) sliced from 3, middle (eid 20) whole, last
        // (eid 10) bounded at 3.
        assert_eq!(t.extract(30, 3, 10, 3).as_deref(), Some("BBBFULLGGG"));
    }

    #[test]
    fn extract_rejects_reversed_or_unknown_handles() {
        let t = idx(&[(10, "abc"), (20, "def")], &[(10, 0), (20, 1)]);
        assert_eq!(t.extract(20, 0, 10, 1), None, "end precedes start");
        assert_eq!(t.extract(10, 0, 99, 1), None, "unknown end eid");
        assert_eq!(t.extract(99, 0, 20, 1), None, "unknown start eid");
    }

    #[test]
    fn extract_clamps_out_of_range_offsets() {
        let t = idx(&[(10, "abc")], &[(10, 0)]);
        // off_end past the end clamps to the element length instead of panicking.
        assert_eq!(t.extract(10, 1, 10, 99).as_deref(), Some("bc"));
    }

    #[test]
    fn position_is_pid_plus_offset() {
        let t = idx(&[(10, "x")], &[(10, 1000)]);
        assert_eq!(t.position(10, 44), Some(1044));
        assert_eq!(t.position(999, 0), None);
    }

    #[test]
    fn collect_eid_text_walks_storyline_tree() {
        // A storyline struct: { $146 content_list: [ {$155 id, $145 "inline"},
        //                                            {$155 id, $146 [ {$155 id, $145 "nested"} ]} ] }
        let leaf = IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Int(2)),
            (KfxSymbol::Content as u64, IonValue::String("nested".into())),
        ]);
        let container = IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Int(3)),
            (KfxSymbol::ContentList as u64, IonValue::List(vec![leaf])),
        ]);
        let inline = IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Int(1)),
            (KfxSymbol::Content as u64, IonValue::String("inline".into())),
        ]);
        let storyline = IonValue::Struct(vec![(
            KfxSymbol::ContentList as u64,
            IonValue::List(vec![inline, container]),
        )]);

        // `resolve_content_text` returns the literal for `IonValue::String`,
        // so we don't need a backing content fragment here.
        let book = loader::empty_book_for_test();
        let mut out = HashMap::new();
        collect_eid_text(&storyline, &book, &mut out);
        assert_eq!(out.get(&1).map(String::as_str), Some("inline"));
        assert_eq!(out.get(&2).map(String::as_str), Some("nested"));
        // eid 3 is a pure container (no `$145 content`) → no text entry.
        assert!(!out.contains_key(&3));
    }
}
