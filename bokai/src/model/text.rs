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
    /// `(element, first character, end)` of each run, in axis order.
    runs: Vec<(i64, usize, Option<usize>)>,
    /// Element → `(first character, index into runs)` of each of its runs, ascending.
    runs_of: HashMap<i64, Vec<(usize, usize)>>,
}

impl SourceText {
    /// Index the text against a position scale; `order` and `runs` leave out an
    /// element the scale does not place, which [`Self::text_of`] keeps.
    pub fn new(text_of: HashMap<i64, String>, positions: &PositionMap) -> Self {
        let mut order: Vec<i64> = positions.positions().keys().copied().collect();
        // Keyed `(position, element)`: a tie orders by element id.
        order.sort_unstable_by_key(|&e| (positions.position(e, 0).unwrap_or(0), e));

        let mut runs: Vec<(i64, usize, Option<usize>)> = Vec::new();
        let mut runs_of: HashMap<i64, Vec<(usize, usize)>> = HashMap::new();
        for (index, (_, element, offset)) in positions.anchor_axis().into_iter().enumerate() {
            let from = offset.max(0) as usize;
            runs.push((element, from, None));
            runs_of.entry(element).or_default().push((from, index));
        }
        for starts in runs_of.values_mut() {
            starts.sort_unstable();
            for pair in starts.windows(2) {
                runs[pair[0].1].2 = Some(pair[1].0);
            }
        }
        Self {
            text_of,
            order,
            runs,
            runs_of,
        }
    }

    /// The base text of one element, if the source gave it any.
    pub fn text_of(&self, element: i64) -> Option<&str> {
        self.text_of.get(&element).map(String::as_str)
    }

    /// Every placed element in reading order, text-bearing or not.
    pub fn reading_order(&self) -> &[i64] {
        &self.order
    }

    /// How many elements are in the ordered walk.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether no element carries text; `reading_order` may hold elements all the same.
    pub fn is_empty(&self) -> bool {
        self.text_of.values().all(|t| t.is_empty())
    }

    /// The text from character `off_start` of `start` to character `off_end` of
    /// `end`, in axis order. `None` where either element is unplaced or the end
    /// precedes the start.
    pub fn extract(
        &self,
        start: i64,
        off_start: usize,
        end: i64,
        off_end: usize,
    ) -> Option<String> {
        let first = self.run_holding(start, off_start)?;
        let last = self.run_holding(end, off_end)?;
        if last < first || (last == first && off_end < off_start) {
            return None;
        }
        let mut out = String::new();
        for (index, &(element, from, to)) in self.runs.iter().enumerate().take(last + 1).skip(first)
        {
            let chars: Vec<char> = self
                .text_of
                .get(&element)
                .map(|t| t.chars().collect())
                .unwrap_or_default();
            let to = to.unwrap_or(chars.len()).min(chars.len());
            let a = if index == first { off_start } else { from }.min(to);
            let b = if index == last { off_end } else { to }.min(to).max(a);
            out.extend(&chars[a..b]);
        }
        Some(out)
    }

    /// Index into `runs` of the run holding character `offset` of `element`.
    fn run_holding(&self, element: i64, offset: usize) -> Option<usize> {
        let starts = self.runs_of.get(&element)?;
        let past = starts.partition_point(|&(from, _)| from <= offset);
        starts.get(past.checked_sub(1)?).map(|&(_, index)| index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three elements whose ids run against reading order.
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

    /// `extract` from a placed element with no text yields the text the range reaches.
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

    /// `reading_order` orders elements tied on position by element id.
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

    /// Element 9 holds `abcdefgh`, and element 12 (`XY`) sits on the axis
    /// between its `d` and `e`.
    fn interrupted() -> SourceText {
        let positions = PositionMap::new(HashMap::from([(9, 50), (12, 54)]), vec![0], None)
            .with_anchors(HashMap::from([(9, vec![(4, 56)])]));
        let text_of = HashMap::from([(9, "abcdefgh".to_string()), (12, "XY".to_string())]);
        SourceText::new(text_of, &positions)
    }

    #[test]
    fn a_range_takes_an_interrupting_element_in_axis_order() {
        let t = interrupted();
        assert_eq!(t.extract(9, 2, 9, 6).as_deref(), Some("cdXYef"));
        assert_eq!(t.extract(9, 2, 12, 1).as_deref(), Some("cdX"));
        assert_eq!(t.extract(12, 1, 9, 6).as_deref(), Some("Yef"));
        assert_eq!(t.extract(9, 1, 9, 3).as_deref(), Some("bc"));
        assert_eq!(t.extract(9, 6, 12, 0), None, "12 precedes the second run");
    }

    #[test]
    fn a_backwards_range_is_rejected_and_bad_offsets_clamp() {
        let t = sample();
        assert_eq!(t.extract(10, 0, 30, 1), None, "end precedes start");
        assert_eq!(
            t.extract(30, 4, 30, 2),
            None,
            "end precedes start in one element"
        );
        assert_eq!(
            t.extract(30, 99, 30, 999).as_deref(),
            Some(""),
            "offsets past the end clamp instead of panicking"
        );
    }
}
