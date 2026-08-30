//! Reading positions — the linear scale a source addresses its text on.

use std::collections::HashMap;

/// A book's reading-position scale: every addressable element's coordinate on
/// the source's own linear axis, plus the boundaries dividing that axis into
/// the numbered "locations" a reading device displays.
#[derive(Debug, Clone, Default)]
pub struct PositionMap {
    /// Source element id → its coordinate on the linear axis.
    position_of: HashMap<i64, i64>,
    /// Location boundaries, ascending. `boundaries[k]` is the coordinate at
    /// which the `(k+1)`-th location starts.
    boundaries: Vec<i64>,
    /// Where the axis ends, one past its last coordinate. The last element's
    /// coordinate is where it *starts*, and the extent sits past it by that
    /// element's own length.
    extent: i64,
    /// Per element, the `(offset, coordinate)` pairs a source states inside
    /// it, ascending. Empty for a source that states none.
    anchors: HashMap<i64, Vec<(i64, i64)>>,
}

impl PositionMap {
    /// Assemble a map from an element→coordinate table and the location
    /// boundaries on the same axis. Boundaries are sorted here, and a caller
    /// hands over whatever order the source listed them in.
    pub fn new(
        position_of: HashMap<i64, i64>,
        mut boundaries: Vec<i64>,
        extent: Option<i64>,
    ) -> Self {
        boundaries.sort_unstable();
        let extent = extent.unwrap_or_else(|| {
            position_of
                .values()
                .copied()
                .max()
                .unwrap_or(0)
                .max(boundaries.last().copied().unwrap_or(0))
        });
        Self {
            position_of,
            anchors: HashMap::new(),
            boundaries,
            extent,
        }
    }

    /// Synthesize a coordinate axis for a source that ships none, measuring it
    /// in characters of the book's own text: each element sits at the running
    /// character total of everything ahead of it in reading order.
    pub fn synthesized(reading_order: &[i64], text_len: impl Fn(i64) -> i64) -> Self {
        let mut position_of = HashMap::with_capacity(reading_order.len());
        let mut cursor: i64 = 0;
        for &element in reading_order {
            if position_of.contains_key(&element) {
                continue;
            }
            position_of.insert(element, cursor);
            cursor += text_len(element).max(0);
        }
        Self {
            position_of,
            anchors: HashMap::new(),
            boundaries: Vec::new(),
            extent: cursor,
        }
    }

    /// Take the mid-element anchors a source states: per element, the
    /// coordinates it gives characters other than its first.
    pub fn with_anchors(mut self, anchors: HashMap<i64, Vec<(i64, i64)>>) -> Self {
        self.anchors = anchors;
        for stated in self.anchors.values_mut() {
            stated.sort_unstable();
        }
        self
    }

    /// The coordinate of a point `offset` characters into `element`, or `None`
    pub fn position(&self, element: i64, offset: i64) -> Option<i64> {
        let start = *self.position_of.get(&element)?;
        let anchor = self.anchors.get(&element).and_then(|stated| {
            let past = stated.partition_point(|(at, _)| *at <= offset);
            past.checked_sub(1).map(|index| stated[index])
        });
        Some(match anchor {
            Some((at, coordinate)) => coordinate + (offset - at),
            None => start + offset,
        })
    }

    /// Whether the source defined the numbered location scale on top of the
    pub fn has_locations(&self) -> bool {
        !self.boundaries.is_empty()
    }

    /// Every positioned element paired with its raw coordinate, ordered by
    /// element id. The axis beneath [`element_locations`](Self::element_locations),
    /// for a consumer that has no location scale to map through.
    pub fn element_positions(&self) -> Vec<(i64, i64)> {
        let mut out: Vec<(i64, i64)> = self
            .position_of
            .iter()
            .map(|(&element, &pos)| (element, pos))
            .collect();
        out.sort_unstable();
        out
    }

    /// The far end of the coordinate axis — the denominator for progress
    /// measured in coordinates, not in locations.
    pub fn max_position(&self) -> i64 {
        self.extent
    }

    /// The location number a coordinate falls in: the count of boundaries
    pub fn location_for(&self, position: i64) -> i64 {
        self.boundaries.partition_point(|&b| b < position).max(1) as i64
    }

    /// How many locations the book has — the "Loc N of M" denominator.
    pub fn location_count(&self) -> i64 {
        self.boundaries.len() as i64
    }

    /// Every positioned element paired with its location number, ordered by
    /// element id, sorted for a reproducible result:
    pub fn element_locations(&self) -> Vec<(i64, i64)> {
        let mut out: Vec<(i64, i64)> = self
            .position_of
            .iter()
            .map(|(&element, &pos)| (element, self.location_for(pos)))
            .collect();
        out.sort_unstable();
        out
    }

    /// Whether the source addressed no elements at all.
    pub fn is_empty(&self) -> bool {
        self.position_of.is_empty()
    }

    /// Number of positioned elements.
    pub fn len(&self) -> usize {
        self.position_of.len()
    }

    /// The element→coordinate table, for consumers that need the raw axis
    /// under the location scale (an annotation resolver orders elements
    /// by coordinate to walk a highlight that spans several).
    pub fn positions(&self) -> &HashMap<i64, i64> {
        &self.position_of
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PositionMap {
        // Three elements at 0/50/300, boundaries every 100.
        let position_of = HashMap::from([(7, 0), (9, 50), (4, 300)]);
        PositionMap::new(position_of, vec![200, 0, 100, 300], None)
    }

    #[test]
    fn offsets_add_to_the_elements_coordinate() {
        let m = sample();
        assert_eq!(m.position(9, 0), Some(50));
        assert_eq!(m.position(9, 12), Some(62));
        assert_eq!(m.position(999, 0), None);
    }

    /// An element whose text a nested element interrupts runs past its own
    #[test]
    fn offsets_past_an_interruption_count_from_the_anchor() {
        let m = sample().with_anchors(HashMap::from([(9, vec![(6, 60), (4, 55)])]));
        assert_eq!(m.position(9, 0), Some(50));
        assert_eq!(m.position(9, 3), Some(53));
        assert_eq!(m.position(9, 4), Some(55));
        assert_eq!(m.position(9, 5), Some(56));
        assert_eq!(m.position(9, 6), Some(60));
        assert_eq!(m.position(9, 8), Some(62));
        // An element the source states no anchors for is unaffected.
        assert_eq!(m.position(7, 9), Some(9));
    }

    #[test]
    fn a_coordinate_on_a_boundary_completes_that_location() {
        let m = sample();
        assert_eq!(m.location_for(0), 1, "floored at 1, never Location 0");
        assert_eq!(m.location_for(50), 1);
        assert_eq!(m.location_for(100), 1, "exactly on boundary #2");
        assert_eq!(m.location_for(101), 2);
        assert_eq!(m.location_count(), 4);
    }

    #[test]
    fn element_locations_are_ordered_by_element() {
        let m = sample();
        assert_eq!(m.element_locations(), vec![(4, 3), (7, 1), (9, 1)]);
    }

    #[test]
    fn a_source_supplied_map_has_locations() {
        assert!(sample().has_locations());
        assert_eq!(
            sample().element_positions(),
            vec![(4, 300), (7, 0), (9, 50)]
        );
    }

    #[test]
    fn a_synthesized_axis_stacks_elements_by_their_text_length() {
        let lengths = HashMap::from([(10, 40), (11, 0), (12, 7)]);
        let m = PositionMap::synthesized(&[10, 11, 12], |e| lengths.get(&e).copied().unwrap_or(0));
        assert_eq!(m.element_positions(), vec![(10, 0), (11, 40), (12, 40)]);
        // Past the last element's start by its own length: the axis ends where
        // the text does.
        assert_eq!(m.max_position(), 47);
    }

    #[test]
    fn a_synthesized_axis_claims_no_locations() {
        let m = PositionMap::synthesized(&[1, 2], |_| 10);
        assert!(
            !m.has_locations(),
            "synthesized coordinates must not pose as a device's location scale"
        );
        assert_eq!(m.location_count(), 0);
    }

    #[test]
    fn a_repeated_element_keeps_its_first_position() {
        // A reading order can name an element twice (it spans a boundary, or a
        // walk revisits it). The second sighting must not move it or advance
        // the axis, or everything after it drifts.
        let m = PositionMap::synthesized(&[1, 2, 1, 3], |_| 10);
        assert_eq!(m.element_positions(), vec![(1, 0), (2, 10), (3, 20)]);
        assert_eq!(m.max_position(), 30);
    }

    #[test]
    fn an_element_with_no_known_text_takes_no_space() {
        let m = PositionMap::synthesized(&[1, 2, 3], |e| if e == 2 { -5 } else { 10 });
        assert_eq!(
            m.element_positions(),
            vec![(1, 0), (2, 10), (3, 10)],
            "a negative length must not walk the axis backwards"
        );
    }
}
