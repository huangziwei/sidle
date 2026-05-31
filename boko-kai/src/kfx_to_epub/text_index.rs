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

/// One occurrence of a search query inside a single reading-order element.
/// Offsets are character indices into the element's base text. `off_end` is
/// the **inclusive** last-char index of the match (matches the annotation
/// `.yjr` convention, so the JS `rangeFor` walk paints the correct range).
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
        // Fixed-layout (PDF-backed) KFX: `position_id_map` is
        // `{contains:[{section_name,pid,length}]}`, not the reflowable
        // `{eid,pid}` list, so the loop above finds nothing. Rebuild `eid→pid`
        // by replaying each `section_position_id_map` walk.
        if pid_of.is_empty() {
            pid_of = fixed_layout_pid_map(&book.by_type);
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

    /// All substring occurrences of `needle` across every indexed element, in
    /// reading order. v1 = strict char match, ASCII case-insensitive only — no
    /// NFKC/kata→hira folding (`「ＡＢＣ」` won't match `abc`; カタカナ won't
    /// match かたかな). v1 = intra-eid only (a match must fit inside one
    /// element); narrative text is the common case. Ruby `<rt>` text never
    /// enters `text_of`, so it's skipped for free.
    ///
    /// Returns at most `MAX_RESULTS` matches; each carries a `preview_*` triple
    /// (up to `PREVIEW_CHARS` chars of context on each side of the match,
    /// drawn from the *original* casing) for the UI's results list. `off_end`
    /// follows the annotation convention — it's the inclusive last-char index,
    /// so JS `rangeFor`'s end-inclusive `+1` walk paints the right characters.
    pub fn search(&self, needle: &str) -> Vec<SearchMatch> {
        const PREVIEW_CHARS: usize = 32;
        const MAX_RESULTS: usize = 1000;
        if needle.is_empty() {
            return Vec::new();
        }
        let needle_lower: Vec<char> = needle.chars().map(|c| c.to_ascii_lowercase()).collect();
        let nlen = needle_lower.len();
        let mut out = Vec::new();

        for &eid in &self.order {
            if out.len() >= MAX_RESULTS {
                break;
            }
            let Some(text) = self.text_of.get(&eid) else {
                continue;
            };
            let chars: Vec<char> = text.chars().collect();
            if chars.len() < nlen {
                continue;
            }
            let lower_chars: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
            let pid = self.pid_of.get(&eid).copied().unwrap_or(0);
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

/// `eid → pid` for a FIXED-LAYOUT (PDF-backed) KFX. There `position_id_map`
/// ($265) is `{contains:[{section_name, pid, length}]}` (one span per section)
/// and every section carries a `section_position_id_map` ($609) with the compact
/// position→eid walk. Map each section to its base pid, then replay the walk —
/// the inverse of boko's `build_pdf_section_position_id_map_fragments`: a
/// `[advance, eid]` pair names an explicit eid; a bare int names the previous
/// eid + 1 (consecutive); `[advance, 0]` terminates at `pid == section length`.
/// Each element advances the running pid by the PREVIOUS element's span (so the
/// advance is already baked into the encoding). Empty for a reflowable book
/// (no $609 fragments).
fn fixed_layout_pid_map(
    by_type: &std::collections::HashMap<u64, HashMap<String, IonValue>>,
) -> HashMap<i64, i64> {
    fn sym_id(v: &IonValue) -> Option<u64> {
        match v.unwrap_annotated() {
            IonValue::Symbol(s) => Some(*s),
            _ => None,
        }
    }

    // section-name symbol → base (absolute) pid, from `position_id_map`.
    let mut base_pid: HashMap<u64, i64> = HashMap::new();
    if let Some(maps) = by_type.get(&(KfxSymbol::PositionIdMap as u64)) {
        for frag in maps.values() {
            let Some(fields) = frag.unwrap_annotated().as_struct() else {
                continue;
            };
            let Some(contains) =
                get_field(fields, KfxSymbol::Contains as u64).and_then(|v| v.as_list())
            else {
                continue;
            };
            for entry in contains {
                let Some(ef) = entry.unwrap_annotated().as_struct() else {
                    continue;
                };
                if let Some(sym) = get_field(ef, KfxSymbol::SectionName as u64).and_then(sym_id)
                    && let Some(pid) = get_field(ef, KfxSymbol::Pid as u64).and_then(|v| v.as_int())
                {
                    base_pid.insert(sym, pid);
                }
            }
        }
    }

    let mut pid_of: HashMap<i64, i64> = HashMap::new();
    let Some(maps) = by_type.get(&(KfxSymbol::SectionPositionIdMap as u64)) else {
        return pid_of;
    };
    for frag in maps.values() {
        let Some(fields) = frag.unwrap_annotated().as_struct() else {
            continue;
        };
        let start = get_field(fields, KfxSymbol::SectionName as u64)
            .and_then(sym_id)
            .and_then(|s| base_pid.get(&s).copied())
            .unwrap_or(0);
        let Some(contains) =
            get_field(fields, KfxSymbol::Contains as u64).and_then(|v| v.as_list())
        else {
            continue;
        };
        let mut pid = start;
        let mut prev_eid: Option<i64> = None;
        for elem in contains {
            let inner = elem.unwrap_annotated();
            if let Some(pair) = inner.as_list() {
                // `[advance, eid]` — explicit eid (`[advance, 0]` = terminator).
                let (Some(advance), Some(eid)) = (
                    pair.first().and_then(|v| v.as_int()),
                    pair.get(1).and_then(|v| v.as_int()),
                ) else {
                    continue;
                };
                pid += advance;
                if eid == 0 {
                    break; // terminator: pid now == section length
                }
                pid_of.insert(eid, pid);
                prev_eid = Some(eid);
            } else if let Some(advance) = inner.as_int() {
                // bare int — consecutive: the previous eid + 1.
                let Some(eid) = prev_eid.map(|e| e + 1) else {
                    continue;
                };
                pid += advance;
                pid_of.insert(eid, pid);
                prev_eid = Some(eid);
            }
        }
    }
    pid_of
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

    // Replays the page-0 `section_position_id_map` walk decoded from Amazon's
    // (and boko's) "The Street Was Mine" KFX — the inverse of
    // `build_pdf_section_position_id_map_fragments`. Section c0 base pid 0,
    // length 97; text-run eids 262/263/264/265 land at pids 4/24/45/78.
    #[test]
    fn fixed_layout_pid_map_replays_section_walk() {
        let sec = 853u64; // a "c0" section-name symbol
        let pidmap = IonValue::Struct(vec![(
            KfxSymbol::Contains as u64,
            IonValue::List(vec![IonValue::Struct(vec![
                (KfxSymbol::SectionName as u64, IonValue::Symbol(sec)),
                (KfxSymbol::Pid as u64, IonValue::Int(0)),
                (KfxSymbol::Length as u64, IonValue::Int(97)),
            ])]),
        )]);
        let pair = |a: i64, e: i64| IonValue::List(vec![IonValue::Int(a), IonValue::Int(e)]);
        let spidmap = IonValue::Struct(vec![
            (KfxSymbol::SectionName as u64, IonValue::Symbol(sec)),
            (
                KfxSymbol::Contains as u64,
                IonValue::List(vec![
                    pair(0, 11635),
                    pair(1, 2),
                    pair(1, 260),
                    IonValue::Int(1),
                    IonValue::Int(1),
                    IonValue::Int(20),
                    IonValue::Int(21),
                    IonValue::Int(33),
                    IonValue::Int(16),
                    IonValue::Int(1),
                    pair(1, 11636),
                    pair(1, 0),
                ]),
            ),
        ]);
        let mut by_type: HashMap<u64, HashMap<String, IonValue>> = HashMap::new();
        by_type
            .entry(KfxSymbol::PositionIdMap as u64)
            .or_default()
            .insert("$348".into(), pidmap);
        by_type
            .entry(KfxSymbol::SectionPositionIdMap as u64)
            .or_default()
            .insert("c0".into(), spidmap);

        let pid = fixed_layout_pid_map(&by_type);
        for (eid, want) in [
            (11635, 0),
            (2, 1),
            (260, 2),
            (261, 3),
            (262, 4),
            (263, 24),
            (264, 45),
            (265, 78),
            (266, 94),
            (267, 95),
            (11636, 96),
        ] {
            assert_eq!(pid.get(&eid), Some(&want), "eid {eid}");
        }
        assert!(!pid.contains_key(&0), "terminator eid 0 must not be recorded");
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
    fn search_finds_cjk_substring_with_char_offsets() {
        // 恥 is at char index 0; 多い at chars 2..4. The byte offsets differ
        // (JP chars are 3 bytes in UTF-8) — search must return CHAR indices to
        // match the (eid,offset) anchor convention.
        let t = idx(&[(10, "恥の多い生涯を送って来ました")], &[(10, 100)]);
        let m = t.search("多い");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].eid, 10);
        assert_eq!(m[0].off_start, 2);
        assert_eq!(m[0].off_end, 3, "inclusive last-char (annotation convention)");
        assert_eq!(m[0].linear_pos, 102);
        assert_eq!(m[0].preview_match, "多い");
        assert_eq!(m[0].preview_before, "恥の");
    }

    #[test]
    fn search_is_ascii_case_insensitive_only() {
        let t = idx(&[(10, "Hello WORLD")], &[(10, 0)]);
        assert_eq!(t.search("hello").len(), 1, "lowercase query matches mixed");
        assert_eq!(t.search("WORLD").len(), 1, "uppercase query matches uppercase");
        assert_eq!(t.search("world").len(), 1, "lowercase query matches uppercase");
        // No non-ASCII folding: fullwidth ＡＢＣ does NOT match ASCII abc.
        let t2 = idx(&[(10, "ＡＢＣ")], &[(10, 0)]);
        assert!(t2.search("abc").is_empty(), "no NFKC folding in v1");
    }

    #[test]
    fn search_returns_multiple_non_overlapping_matches_per_eid() {
        let t = idx(&[(10, "abababab")], &[(10, 0)]);
        let m = t.search("ab");
        assert_eq!(m.len(), 4);
        assert_eq!(m.iter().map(|x| x.off_start).collect::<Vec<_>>(), vec![0, 2, 4, 6]);
        // Stepping by needle length avoids reporting overlapping `aa` inside `aaa`.
        let t2 = idx(&[(10, "aaaaa")], &[(10, 0)]);
        let m2 = t2.search("aa");
        assert_eq!(m2.len(), 2, "non-overlapping: aa at 0, aa at 2");
        assert_eq!(m2.iter().map(|x| x.off_start).collect::<Vec<_>>(), vec![0, 2]);
    }

    #[test]
    fn search_orders_results_by_linear_pos_across_eids() {
        // pid order is 5,6,7 → eids 30,20,10 (so out-of-eid-numeric-order, like
        // the existing extract test). Both have the needle; results must come
        // in pid order with linear_pos = pid + off.
        let t = idx(
            &[(10, "foo BAR baz"), (20, "no hits here"), (30, "first bar match")],
            &[(30, 5), (20, 10), (10, 20)],
        );
        let m = t.search("bar");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].eid, 30, "earlier pid first");
        assert_eq!(m[0].linear_pos, 5 + 6);
        assert_eq!(m[1].eid, 10);
        assert_eq!(m[1].linear_pos, 20 + 4);
    }

    #[test]
    fn search_truncates_preview_at_element_boundaries() {
        // Match at the very start of an element: preview_before is empty.
        let t = idx(&[(10, "abc def")], &[(10, 0)]);
        let m = t.search("abc");
        assert_eq!(m[0].preview_before, "");
        assert!(m[0].preview_after.starts_with(" def"));
        // Match at the very end: preview_after is empty.
        let m2 = t.search("def");
        assert_eq!(m2[0].preview_after, "");
        assert!(m2[0].preview_before.ends_with("abc "));
    }

    #[test]
    fn search_ignores_empty_query_and_too_long_query() {
        let t = idx(&[(10, "abc")], &[(10, 0)]);
        assert!(t.search("").is_empty(), "empty query → no matches");
        assert!(t.search("abcdef").is_empty(), "needle longer than any text");
    }

    #[test]
    fn search_skips_ruby_because_text_of_already_does() {
        // text_of holds only base text (per collect_eid_text). Searching for
        // ruby-only characters that aren't in the base run finds nothing —
        // confirms the "ruby skipped for free" claim in the plan.
        let t = idx(&[(10, "恥の多い生涯")], &[(10, 0)]);
        assert!(t.search("はじ").is_empty(), "the ruby reading 'はじ' is not in base text_of");
        assert_eq!(t.search("恥").len(), 1, "base text 恥 is findable");
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
