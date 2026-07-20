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
}

impl PositionMap {
    /// Assemble a map from an element→coordinate table and the location
    /// boundaries on the same axis. Boundaries are sorted here so callers can
    /// hand over whatever order the source listed them in.
    pub fn new(position_of: HashMap<i64, i64>, mut boundaries: Vec<i64>) -> Self {
        boundaries.sort_unstable();
        Self {
            position_of,
            boundaries,
        }
    }

    /// The coordinate of a point `offset` characters into `element`, or `None`
    /// when the element has no position (it is not part of the source's
    /// addressable text).
    pub fn position(&self, element: i64, offset: i64) -> Option<i64> {
        self.position_of.get(&element).map(|p| p + offset)
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
}
