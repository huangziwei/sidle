//! The source's own text, keyed by the source's own element ids.

use std::collections::HashMap;

use crate::model::PositionMap;

/// A book's base text as the *source* stores it: for each addressable element,
/// the characters that element contributes, with nothing added.
#[derive(Debug, Clone, Default)]
pub struct SourceText {
    text_of: HashMap<i64, String>,
    /// Every element the scale places, in position order — text-bearing or not.
    order: Vec<i64>,
    /// Element → its index in `order`.
    rank: HashMap<i64, usize>,
}

impl SourceText {
    /// Index the text against a position scale. Elements the scale does not
    /// place are kept for direct lookup but stay out of the ordered walk —
    /// nothing can be said about where they sit relative to the rest.
    pub fn new(text_of: HashMap<i64, String>, positions: &PositionMap) -> Self {
        let mut order: Vec<i64> = positions.positions().keys().copied().collect();
        // Keyed `(position, element)`: a HashMap's iteration order is not
        order.sort_unstable_by_key(|&e| (positions.position(e, 0).unwrap_or(0), e));
        let rank = order.iter().enumerate().map(|(i, &e)| (e, i)).collect();
        Self {
            text_of,
            order,
            rank,
        }
    }

    /// The base text of one element, if the source gave it any.
    pub fn text_of(&self, element: i64) -> Option<&str> {
        self.text_of.get(&element).map(String::as_str)
    }

    /// Every placed element in reading order. Callers wanting only the ones
    /// with words filter on [`Self::text_of`].
    pub fn reading_order(&self) -> &[i64] {
        &self.order
    }

    /// How many elements are in the ordered walk.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether no element carries text — an image-only book, or one whose
    /// source text was unavailable. The ordered walk may still be populated:
    /// positions exist independently of text.
    pub fn is_empty(&self) -> bool {
        self.text_of.values().all(|t| t.is_empty())
    }

    /// The text a range spans: a reading-order walk from `start` to `end`
    pub fn extract(
        &self,
        start: i64,
        off_start: usize,
        end: i64,
        off_end: usize,
    ) -> Option<String> {
        let &i = self.rank.get(&start)?;
        let &j = self.rank.get(&end)?;
        if j < i {
            return None;
        }
        let mut out = String::new();
        for k in i..=j {
            let element = self.order[k];
            let chars: Vec<char> = self
                .text_of
                .get(&element)
                .map(|t| t.chars().collect())
                .unwrap_or_default();
            let a = if k == i { off_start } else { 0 };
            let b = if k == j { off_end } else { chars.len() };
            let a = a.min(chars.len());
            let b = b.min(chars.len()).max(a);
            out.extend(&chars[a..b]);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three elements whose ids run *against* reading order, so anything that
    /// leans on id ordering instead of the position scale shows up.
    fn sample() -> SourceText {
        let positions =
            PositionMap::new(HashMap::from([(30, 0), (20, 10), (10, 20)]), vec![0], None);
        let text_of = HashMap::from([
            (30, "alpha".to_string()),
            (20, "beta".to_string()),
            (10, "gamma".to_string()),
            (99, "unplaced".to_string()),
        ]);
        SourceText::new(text_of, &positions)
    }

    #[test]
    fn reading_order_follows_positions_not_element_ids() {
        assert_eq!(sample().reading_order(), &[30, 20, 10]);
    }

    #[test]
    fn an_unplaced_element_still_resolves_but_stays_out_of_the_walk() {
        let t = sample();
        assert_eq!(t.text_of(99), Some("unplaced"));
        assert_eq!(t.len(), 3);
        assert_eq!(t.extract(99, 0, 99, 1), None);
    }

    #[test]
    fn a_range_walks_whole_elements_between_its_endpoints() {
        let t = sample();
        assert_eq!(t.extract(30, 0, 30, 5).as_deref(), Some("alpha"));
        assert_eq!(t.extract(30, 4, 20, 2).as_deref(), Some("abe"));
        assert_eq!(t.extract(30, 0, 10, 5).as_deref(), Some("alphabetagamma"));
    }

    /// A Kindle anchors a highlight at whichever element holds the boundary,
    #[test]
    fn a_range_starting_on_a_textless_element_still_yields_its_text() {
        let positions = PositionMap::new(
            HashMap::from([(500, 0), (501, 1), (502, 103)]),
            vec![0],
            None,
        );
        // 500 and 501 are placed section/heading wrappers with no text.
        let text_of = HashMap::from([(502, "Q: How many voters?".to_string())]);
        let t = SourceText::new(text_of, &positions);

        assert_eq!(
            t.reading_order(),
            &[500, 501, 502],
            "textless ones are placed"
        );
        assert_eq!(
            t.extract(501, 0, 502, 19).as_deref(),
            Some("Q: How many voters?"),
            "a textless start contributes nothing, it does not void the range",
        );
        assert_eq!(
            t.extract(500, 0, 501, 0).as_deref(),
            Some(""),
            "a range wholly inside textless elements is empty, not absent",
        );
    }

    /// Equal positions must not leave reading order at the mercy of HashMap
    /// iteration — a range's text would change between runs.
    #[test]
    fn ties_on_position_order_by_element_id() {
        let positions = PositionMap::new(HashMap::from([(7, 5), (3, 5), (9, 5)]), vec![0], None);
        let text_of = HashMap::from([
            (3, "a".to_string()),
            (7, "b".to_string()),
            (9, "c".to_string()),
        ]);
        for _ in 0..8 {
            let t = SourceText::new(text_of.clone(), &positions);
            assert_eq!(t.reading_order(), &[3, 7, 9]);
            assert_eq!(t.extract(3, 0, 9, 1).as_deref(), Some("abc"));
        }
    }

    #[test]
    fn a_backwards_range_is_rejected_and_bad_offsets_clamp() {
        let t = sample();
        assert_eq!(t.extract(10, 0, 30, 1), None, "end precedes start");
        assert_eq!(
            t.extract(30, 99, 30, 999).as_deref(),
            Some(""),
            "offsets past the end clamp instead of panicking"
        );
    }
}
