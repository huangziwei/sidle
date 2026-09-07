//! Reading positions — the linear scale a source addresses its text on.

use std::collections::HashMap;

/// A book's reading-position scale: every addressable element's coordinate on
/// the source's own linear axis, plus the `boundaries` dividing that axis into
/// numbered locations.
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
    /// boundaries on the same axis. `boundaries` is sorted here, in whatever
    /// order the source listed it.
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

    /// The coordinate of a point `offset` characters into `element`.
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
    /// coordinate axis. False for a [`synthesized`](Self::synthesized) map.
    pub fn has_locations(&self) -> bool {
        !self.boundaries.is_empty()
    }

    /// Every positioned element paired with its raw coordinate, ordered by
    /// element id. The axis beneath [`element_locations`](Self::element_locations).
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

    /// The location number a coordinate falls in: the count of boundaries at
    /// or before it, floored at 1. A coordinate on `boundaries[k]` opens the
    /// `(k+1)`-th location.
    pub fn location_for(&self, position: i64) -> i64 {
        self.boundaries.partition_point(|&b| b <= position).max(1) as i64
    }

    /// How many locations the book has — the "Loc N of M" denominator.
    pub fn location_count(&self) -> i64 {
        self.boundaries.len() as i64
    }

    /// Every positioned element paired with its location number, ordered by
    /// element id.
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

    /// The element→coordinate table under the location scale.
    pub fn positions(&self) -> &HashMap<i64, i64> {
        &self.position_of
    }

    /// The `{element, offset}` coordinate a position on the axis names — the
    /// inverse of [`Self::position`]. The position belongs to the last point
    /// `anchor_axis` states at or before it, with `offset` counted from there.
    pub fn resolve(&self, position: i64) -> Option<(i64, i64)> {
        let anchors = self.anchor_axis();
        let past = anchors.partition_point(|&(at, _, _)| at <= position);
        let &(at, element, offset) = anchors.get(past.checked_sub(1)?)?;
        Some((element, offset + (position - at)))
    }

    /// Every point the source states on the axis as `(position, element,
    /// offset)`, ascending: each `position_of` entry, plus each `anchors` one.
    fn anchor_axis(&self) -> Vec<(i64, i64, i64)> {
        let mut axis: Vec<(i64, i64, i64)> = self
            .position_of
            .iter()
            .map(|(&element, &at)| (at, element, 0))
            .collect();
        for (&element, stated) in &self.anchors {
            axis.extend(stated.iter().map(|&(offset, at)| (at, element, offset)));
        }
        axis.sort_unstable();
        axis
    }
}

/// A point on a book's reading axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coordinate {
    /// The position on the source's own linear axis.
    pub position: i64,
    /// The element id and character offset into that element's base text the
    /// position resolves to, for a source that addresses its text by element.
    /// `None` where the axis indexes the text directly.
    pub element: Option<(i64, i64)>,
}

/// The source text a reading coordinate names, with the coordinates it
/// resolved to.
#[derive(Debug, Clone)]
pub struct PositionText {
    /// Where the returned text begins.
    pub from: Coordinate,
    /// Where it ends, one past its last position.
    pub to: Coordinate,
    /// The source's own text between them.
    pub text: String,
    /// Character index into `text` of the single position asked for. `None`
    /// for a span, whose two ends are `from` and `to`.
    pub mark: Option<usize>,
}

/// A slice of a reading axis that indexes its source's text directly, stating
/// no element ids. See [`crate::import::Importer::axis_slice`].
#[derive(Debug, Clone)]
pub struct AxisSlice {
    /// Where the returned text begins on the axis.
    pub from: i64,
    /// Where it ends, one past its last position.
    pub to: i64,
    /// Where the axis ends, as the source declares it.
    pub extent: i64,
    /// The source's own text between `from` and `to`.
    pub text: String,
    /// Character index into `text` of the single position asked for.
    pub mark: Option<usize>,
}

/// Why a reading coordinate resolved to no text. Each variant names the stage
/// that failed.
#[derive(Debug)]
pub enum PositionError {
    /// The source defines no reading axis.
    NoAxis,
    /// The end of a span precedes its start.
    Inverted { start: i64, end: i64 },
    /// The position lies past the end of the axis.
    OutOfRange { position: i64, extent: i64 },
    /// The axis places no element at the position.
    Unplaced { position: i64 },
    /// The axis places no element of that id.
    NoElement { element: i64 },
    /// The axis indexes its text directly and states no element ids.
    NotElementAddressed { element: i64, offset: i64 },
    /// The source states no text for any element it places.
    NoSourceText,
    /// The element the position names carries no text of its own — an image,
    /// a rule, a page template.
    Untexted { element: i64 },
    /// Reading the source failed.
    Io(std::io::Error),
}

impl std::fmt::Display for PositionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAxis => write!(f, "this book states no reading positions"),
            Self::Inverted { start, end } => write!(f, "end {end} precedes start {start}"),
            Self::OutOfRange { position, extent } => {
                write!(
                    f,
                    "position {position} is past the end of the axis ({extent})"
                )
            }
            Self::Unplaced { position } => {
                write!(f, "the position map places nothing at {position}")
            }
            Self::NoElement { element } => {
                write!(f, "the position map places no element {element}")
            }
            Self::NotElementAddressed { element, offset } => write!(
                f,
                "this book addresses its text directly and has no element {element}:{offset}"
            ),
            Self::NoSourceText => write!(f, "this book states no text for its positions"),
            Self::Untexted { element } => write!(f, "element {element} carries no text"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PositionError {}

impl From<std::io::Error> for PositionError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// How a coordinate is given: a position on the source's linear axis, or the
/// `{element, offset}` pair a navigational target states — the form an anchor
/// and a table-of-contents entry record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    /// A position on the source's own linear axis.
    Position(i64),
    /// An element id and a character offset into that element's base text.
    Element { element: i64, offset: i64 },
}

impl Point {
    /// The `{element, offset}` pair `self` names, resolved through `map` for a
    /// [`Point::Position`].
    fn against(self, map: &PositionMap) -> Result<(i64, i64), PositionError> {
        match self {
            Self::Position(position) => map
                .resolve(position)
                .ok_or(PositionError::Unplaced { position }),
            Self::Element { element, offset } => map
                .positions()
                .contains_key(&element)
                .then_some((element, offset))
                .ok_or(PositionError::NoElement { element }),
        }
    }

    /// The position on the linear axis, for a [`Point::Position`].
    fn on_axis(self) -> Result<i64, PositionError> {
        match self {
            Self::Position(position) => Ok(position),
            Self::Element { element, offset } => {
                Err(PositionError::NotElementAddressed { element, offset })
            }
        }
    }
}

impl crate::model::Book {
    /// The source text at a reading position, or across the span between two
    /// of them. `end` of `None` returns the unit `start` lands in with `mark`
    /// set; two points return `start..end`.
    pub fn text_at(
        &mut self,
        start: Point,
        end: Option<Point>,
    ) -> Result<PositionText, PositionError> {
        match self.position_map() {
            Some(map) if !map.is_empty() => self.text_by_element(&map, start, end),
            _ => self.text_by_axis(start.on_axis()?, end.map(Point::on_axis).transpose()?),
        }
    }

    /// Resolve through an element-addressed axis: point → `{element, offset}`
    /// → that element's base text.
    fn text_by_element(
        &mut self,
        map: &PositionMap,
        start: Point,
        end: Option<Point>,
    ) -> Result<PositionText, PositionError> {
        let extent = map.max_position();
        for point in [Some(start), end].into_iter().flatten() {
            if let Point::Position(position) = point
                && (position < 0 || position > extent)
            {
                return Err(PositionError::OutOfRange { position, extent });
            }
        }
        let (element, offset) = start.against(map)?;
        let text = self.source_text().ok_or(PositionError::NoSourceText)?;

        let Some(end) = end else {
            let body = text
                .text_of(element)
                .filter(|t| !t.is_empty())
                .ok_or(PositionError::Untexted { element })?;
            let characters = body.chars().count() as i64;
            return Ok(PositionText {
                from: coordinate(map, element, 0),
                to: coordinate(map, element, characters),
                text: body.to_string(),
                mark: Some(offset.clamp(0, characters) as usize),
            });
        };

        let (last, last_offset) = end.against(map)?;
        let spanned = text
            .extract(
                element,
                offset.max(0) as usize,
                last,
                last_offset.max(0) as usize,
            )
            .ok_or(PositionError::Inverted {
                start: map.position(element, offset).unwrap_or(0),
                end: map.position(last, last_offset).unwrap_or(0),
            })?;
        Ok(PositionText {
            from: coordinate(map, element, offset),
            to: coordinate(map, last, last_offset),
            text: spanned,
            mark: None,
        })
    }

    /// Resolve through an axis that indexes the source's text directly.
    fn text_by_axis(
        &mut self,
        start: i64,
        end: Option<i64>,
    ) -> Result<PositionText, PositionError> {
        let slice = self.axis_slice(start, end)?.ok_or(PositionError::NoAxis)?;
        if start < 0 || start >= slice.extent {
            return Err(PositionError::OutOfRange {
                position: start,
                extent: slice.extent,
            });
        }
        if let Some(end) = end
            && end > slice.extent
        {
            return Err(PositionError::OutOfRange {
                position: end,
                extent: slice.extent,
            });
        }
        Ok(PositionText {
            from: Coordinate {
                position: slice.from,
                element: None,
            },
            to: Coordinate {
                position: slice.to,
                element: None,
            },
            text: slice.text,
            mark: slice.mark,
        })
    }
}

/// The `Coordinate` of `offset` characters into `element`.
fn coordinate(map: &PositionMap, element: i64, offset: i64) -> Coordinate {
    Coordinate {
        position: map.position(element, offset).unwrap_or(0),
        element: Some((element, offset)),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::import::{ChapterId, Importer, SpineEntry};
    use crate::model::{Book, Landmark, Metadata, SourceText, TocEntry};

    /// A point on the linear axis.
    fn at(position: i64) -> Point {
        Point::Position(position)
    }

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
    /// character count on the axis.
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
    fn a_coordinate_resolves_back_to_the_element_it_names() {
        let m = sample();
        assert_eq!(m.resolve(0), Some((7, 0)));
        assert_eq!(m.resolve(49), Some((7, 49)));
        assert_eq!(m.resolve(50), Some((9, 0)));
        assert_eq!(m.resolve(62), Some((9, 12)));
        assert_eq!(m.resolve(300), Some((4, 0)));
        assert_eq!(m.resolve(-1), None, "before the axis begins");
    }

    /// The positions a nested element occupies fall between two characters of
    /// the element it interrupts. They resolve to the nested element, and the
    /// offsets past it count from the re-entry.
    #[test]
    fn an_interruption_owns_the_positions_it_occupies() {
        let m = sample().with_anchors(HashMap::from([(9, vec![(6, 60), (4, 55)])]));
        assert_eq!(m.resolve(53), Some((9, 3)));
        assert_eq!(m.resolve(55), Some((9, 4)));
        assert_eq!(m.resolve(60), Some((9, 6)));
        assert_eq!(m.resolve(62), Some((9, 8)));
        for offset in [0, 3, 4, 5, 6, 8] {
            let at = m.position(9, offset).expect("element 9 is placed");
            assert_eq!(m.resolve(at), Some((9, offset)), "offset {offset}");
        }
    }

    #[test]
    fn a_coordinate_on_a_boundary_opens_that_location() {
        let m = sample();
        assert_eq!(m.location_for(0), 1, "floored at 1, never Location 0");
        assert_eq!(m.location_for(50), 1);
        assert_eq!(m.location_for(100), 2, "exactly on boundary #2");
        assert_eq!(m.location_for(101), 2);
        assert_eq!(m.location_for(300), 4);
        assert_eq!(m.location_count(), 4);
    }

    #[test]
    fn element_locations_are_ordered_by_element() {
        let m = sample();
        assert_eq!(m.element_locations(), vec![(4, 4), (7, 1), (9, 1)]);
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
        // A reading order can name an element twice. The second sighting must
        // not move it or advance the axis.
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
    /// A source stating whichever of `positions`, `text` and `stream` a test
    /// needs.
    #[derive(Default)]
    struct Source {
        positions: Option<PositionMap>,
        text: Option<SourceText>,
        stream: Option<AxisSlice>,
        metadata: Metadata,
        empty_toc: Vec<TocEntry>,
        empty_landmarks: Vec<Landmark>,
        empty_spine: Vec<SpineEntry>,
        empty_assets: Vec<PathBuf>,
    }

    impl Importer for Source {
        fn open(_: &Path) -> std::io::Result<Self> {
            unreachable!("the test builds the source directly")
        }
        fn metadata(&self) -> &Metadata {
            &self.metadata
        }
        fn toc(&self) -> &[TocEntry] {
            &self.empty_toc
        }
        fn toc_mut(&mut self) -> &mut [TocEntry] {
            &mut self.empty_toc
        }
        fn landmarks(&self) -> &[Landmark] {
            &self.empty_landmarks
        }
        fn landmarks_mut(&mut self) -> &mut [Landmark] {
            &mut self.empty_landmarks
        }
        fn spine(&self) -> &[SpineEntry] {
            &self.empty_spine
        }
        fn source_id(&self, _: ChapterId) -> Option<&str> {
            None
        }
        fn load_raw(&mut self, _: ChapterId) -> std::io::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn list_assets(&self) -> &[PathBuf] {
            &self.empty_assets
        }
        fn load_asset(&mut self, _: &Path) -> std::io::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn position_map(&mut self) -> Option<PositionMap> {
            self.positions.clone()
        }
        fn source_text(&mut self) -> Option<SourceText> {
            self.text.clone()
        }
        fn axis_slice(&mut self, _: i64, _: Option<i64>) -> std::io::Result<Option<AxisSlice>> {
            Ok(self.stream.clone())
        }
    }

    /// Three elements on a stated axis: 7 at 0 ("first one."), 9 at 10
    /// ("second one."), 4 at 30 ("third.").
    fn by_element() -> Book {
        let positions =
            PositionMap::new(HashMap::from([(7, 0), (9, 10), (4, 30)]), vec![0], Some(36));
        let text = HashMap::from([
            (7, "first one.".to_string()),
            (9, "second one.".to_string()),
            (4, "third.".to_string()),
        ]);
        let text = SourceText::new(text, &positions);
        Book::from_importer(Box::new(Source {
            positions: Some(positions),
            text: Some(text),
            ..Source::default()
        }))
    }

    #[test]
    fn a_position_returns_the_element_it_lands_in() {
        let found = by_element()
            .text_at(at(14), None)
            .expect("14 is on the axis");
        assert_eq!(
            found.from,
            Coordinate {
                position: 10,
                element: Some((9, 0))
            }
        );
        assert_eq!(
            found.to,
            Coordinate {
                position: 21,
                element: Some((9, 11))
            }
        );
        assert_eq!(found.text, "second one.");
        assert_eq!(found.mark, Some(4));
    }

    #[test]
    fn a_span_runs_from_one_end_to_the_other() {
        let found = by_element()
            .text_at(at(6), Some(at(33)))
            .expect("both ends are on it");
        assert_eq!(found.from.element, Some((7, 6)));
        assert_eq!(found.to.element, Some((4, 3)));
        assert_eq!(
            found.text, "one.second one.thi",
            "the span crosses three elements"
        );
        assert_eq!(found.mark, None);
    }

    /// The end of a span is exclusive.
    #[test]
    fn a_span_excludes_its_end() {
        let mut book = by_element();
        assert_eq!(
            book.text_at(at(10), Some(at(15))).expect("a span").text,
            "secon"
        );
        assert_eq!(
            book.text_at(at(10), Some(at(10)))
                .expect("an empty span")
                .text,
            ""
        );
    }

    #[test]
    fn each_stage_that_fails_names_itself() {
        let mut book = by_element();
        assert!(matches!(
            book.text_at(at(37), None),
            Err(PositionError::OutOfRange {
                position: 37,
                extent: 36
            })
        ));
        assert!(matches!(
            book.text_at(at(-1), None),
            Err(PositionError::OutOfRange { .. })
        ));
        assert!(matches!(
            book.text_at(at(20), Some(at(4))),
            Err(PositionError::Inverted { start: 20, end: 4 })
        ));
        // Before the first element the axis places nothing to land on.
        let mut gapped = Book::from_importer(Box::new(Source {
            positions: Some(PositionMap::new(HashMap::from([(7, 5)]), vec![0], Some(9))),
            text: Some(SourceText::default()),
            ..Source::default()
        }));
        assert!(matches!(
            gapped.text_at(at(2), None),
            Err(PositionError::Unplaced { position: 2 })
        ));
    }

    /// An element with no text of its own reports `Untexted`.
    #[test]
    fn an_element_carrying_no_text_is_not_an_empty_answer() {
        let positions = PositionMap::new(HashMap::from([(7, 0)]), vec![0], Some(4));
        let text = SourceText::new(HashMap::new(), &positions);
        let mut book = Book::from_importer(Box::new(Source {
            positions: Some(positions),
            text: Some(text),
            ..Source::default()
        }));
        assert!(matches!(
            book.text_at(at(1), None),
            Err(PositionError::Untexted { element: 7 })
        ));
    }

    /// A source addressing its text directly answers with no element at all,
    /// and one stating neither axis has no addressable positions.
    #[test]
    fn a_direct_axis_names_no_element() {
        let mut book = Book::from_importer(Box::new(Source {
            stream: Some(AxisSlice {
                from: 100,
                to: 140,
                extent: 500,
                text: "the record the position landed in.".to_string(),
                mark: Some(7),
            }),
            ..Source::default()
        }));
        let found = book.text_at(at(107), None).expect("107 is on the axis");
        assert_eq!(
            found.from,
            Coordinate {
                position: 100,
                element: None
            }
        );
        assert_eq!(
            found.to,
            Coordinate {
                position: 140,
                element: None
            }
        );
        assert_eq!(found.mark, Some(7));

        assert!(matches!(
            book.text_at(at(500), None),
            Err(PositionError::OutOfRange {
                position: 500,
                extent: 500
            })
        ));
        assert!(matches!(
            Book::from_importer(Box::new(Source::default())).text_at(at(0), None),
            Err(PositionError::NoAxis)
        ));
    }
}
