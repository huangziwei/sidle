//! Reading positions — the linear scale a source addresses its text on.
//!
//! This is the counterpart to [`crate::model::InternalLocation`]'s split. That
//! type names *where a link points*; this one names *how far into the book a
//! point sits*, on the scale the source itself defines.

use std::collections::HashMap;

/// A book's reading-position scale: every addressable element's coordinate on
/// the source's own linear axis, plus the boundaries dividing that axis into
/// the numbered "locations" a reading device displays.
///
/// Physically-addressed formats (the Kindle family) ship both halves — KFX
/// carries an element→coordinate map and a boundary list, and the device turns
/// a coordinate into "Location N" by counting boundaries below it.
/// Structurally-addressed formats (EPUB) ship neither: their readers
/// synthesize progress from the spine, which is a consumer's policy rather
/// than a fact in the file, so those importers report no map at all.
///
/// Elements are keyed by **the source's own identifier** — a KFX `eid`. That
/// is the identifier a device writes into an annotation, so carrying it
/// verbatim is what lets a highlight made on hardware resolve against a book
/// read through the IR.
#[derive(Debug, Clone, Default)]
pub struct PositionMap {
    /// Source element id → its coordinate on the linear axis.
    position_of: HashMap<i64, i64>,
    /// Location boundaries, ascending. `boundaries[k]` is the coordinate at
    /// which the `(k+1)`-th location starts.
    boundaries: Vec<i64>,
    /// Where the axis ends. The last element's coordinate is where it *starts*,
    /// so the extent is past it by that element's own length — a distinction
    /// that only matters to a consumer measuring progress in coordinates.
    extent: i64,
}

impl PositionMap {
    /// Assemble a map from an element→coordinate table and the location
    /// boundaries on the same axis. Boundaries are sorted here so callers can
    /// hand over whatever order the source listed them in.
    pub fn new(position_of: HashMap<i64, i64>, mut boundaries: Vec<i64>) -> Self {
        boundaries.sort_unstable();
        // Nothing here knows the last element's length, so the axis is taken to
        // end at the furthest coordinate named. Sources that ship boundaries
        // carry the real extent in those instead.
        let extent = position_of
            .values()
            .copied()
            .max()
            .unwrap_or(0)
            .max(boundaries.last().copied().unwrap_or(0));
        Self {
            position_of,
            boundaries,
            extent,
        }
    }

    /// Synthesize a coordinate axis for a source that ships none, measuring it
    /// in characters of the book's own text: each element sits at the running
    /// character total of everything ahead of it in reading order.
    ///
    /// No location boundaries come out of this, and
    /// [`has_locations`](Self::has_locations) stays false. Dividing an axis
    /// into numbered locations is a device convention, and a source that never
    /// defined one has no numbering to reproduce — inventing boundaries would
    /// produce a "Location 407" that looks like a device's and matches
    /// nothing. Progress against [`max_position`](Self::max_position) is
    /// faithful, which is what a progress readout actually needs.
    ///
    /// `reading_order` is every addressable element in presentation order;
    /// repeats keep their first position. `text_len` gives an element's own
    /// base-text length — elements it doesn't know occupy no space.
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
            boundaries: Vec::new(),
            extent: cursor,
        }
    }

    /// The coordinate of a point `offset` characters into `element`, or `None`
    /// when the element has no position (it is not part of the source's
    /// addressable text).
    pub fn position(&self, element: i64, offset: i64) -> Option<i64> {
        self.position_of.get(&element).map(|p| p + offset)
    }

    /// Whether the source defined the numbered location scale on top of the
    /// coordinate axis. False for a [`synthesized`](Self::synthesized) map,
    /// whose coordinates are real but whose locations would be invented — a
    /// consumer showing progress should read
    /// [`element_positions`](Self::element_positions) against
    /// [`max_position`](Self::max_position) instead.
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
    /// measured in coordinates rather than locations.
    pub fn max_position(&self) -> i64 {
        self.extent
    }

    /// The location number a coordinate falls in: the count of boundaries
    /// strictly below it. A coordinate sitting exactly on a boundary reads as
    /// the location it *completes*, not the one it starts — the device's own
    /// convention. Floored at 1, so the first page never reads "Location 0".
    pub fn location_for(&self, position: i64) -> i64 {
        self.boundaries.partition_point(|&b| b < position).max(1) as i64
    }

    /// How many locations the book has — the "Loc N of M" denominator.
    pub fn location_count(&self) -> i64 {
        self.boundaries.len() as i64
    }

    /// Every positioned element paired with its location number, ordered by
    /// element id. Sorted rather than hash-ordered so the result is
    /// reproducible: consumers key into it, so the order carries no meaning,
    /// but an API returning a different vector each call cannot be cached,
    /// diffed, or tested.
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
    /// rather than the location scale (an annotation resolver orders elements
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
        PositionMap::new(position_of, vec![200, 0, 100, 300])
    }

    #[test]
    fn offsets_add_to_the_elements_coordinate() {
        let m = sample();
        assert_eq!(m.position(9, 0), Some(50));
        assert_eq!(m.position(9, 12), Some(62));
        assert_eq!(m.position(999, 0), None);
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
