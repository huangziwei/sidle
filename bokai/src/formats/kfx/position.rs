//! Reading positions: the KFX `eid → pid → device Location` chain, assembled
//! from `position_id_map` ($265), `section_position_id_map` ($609) and
//! `location_map` ($550) / `yj.location_pid_map` ($621) into a [`PositionMap`].
//!
//! [`PositionFragments::section_walks`], [`PositionFragments::location_anchors`]
//! and [`PositionFragments::location_pids`] hand back the same fragments
//! unreconciled, for a caller reading them against each other.

use std::collections::HashMap;

use crate::formats::kfx::container::get_field;
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::loader::BookData;
use crate::formats::kfx::symbols::KfxSymbol;
use crate::model::PositionMap;

/// Location boundary spacing in raw pids — kfxlib's own
/// `KFX_POSITIONS_PER_LOCATION`, the spacing for a book that ships a position
/// map and no `location_map`.
const KFX_POSITIONS_PER_LOCATION: i64 = 110;

/// The container fragments the position chain is built from, gathered by type.
/// [`Self::from_book`] fills them from a loaded [`BookData`]; [`Self::push`]
/// takes entities read straight out of a container.
#[derive(Default)]
pub struct PositionFragments<'a> {
    position_id_maps: Vec<&'a IonValue>,
    section_position_id_maps: Vec<&'a IonValue>,
    location_maps: Vec<&'a IonValue>,
    location_pid_maps: Vec<&'a IonValue>,
}

impl<'a> PositionFragments<'a> {
    /// The fragment types the chain reads. A caller scanning a container's
    /// entity index only needs to parse entities of these types.
    pub const TYPES: [KfxSymbol; 4] = [
        KfxSymbol::PositionIdMap,
        KfxSymbol::SectionPositionIdMap,
        KfxSymbol::LocationMap,
        KfxSymbol::YjLocationPidMap,
    ];

    /// Whether a fragment type belongs to the chain.
    pub fn wants(type_id: u32) -> bool {
        Self::TYPES.iter().any(|t| *t as u32 == type_id)
    }

    /// File a parsed fragment under its type. Types outside [`Self::TYPES`]
    /// are ignored.
    pub fn push(&mut self, type_id: u32, fragment: &'a IonValue) {
        let bucket = if type_id == KfxSymbol::PositionIdMap as u32 {
            &mut self.position_id_maps
        } else if type_id == KfxSymbol::SectionPositionIdMap as u32 {
            &mut self.section_position_id_maps
        } else if type_id == KfxSymbol::LocationMap as u32 {
            &mut self.location_maps
        } else if type_id == KfxSymbol::YjLocationPidMap as u32 {
            &mut self.location_pid_maps
        } else {
            return;
        };
        bucket.push(fragment);
    }

    /// Every fragment of a loaded book, filed by type.
    pub fn from_book(book: &'a BookData) -> Self {
        let mut out = Self::default();
        for symbol in Self::TYPES {
            if let Some(frags) = book.by_type.get(&(symbol as u64)) {
                for frag in frags.values() {
                    out.push(symbol as u32, frag);
                }
            }
        }
        out
    }

    /// The `eid → pid` map from `position_id_map` ($265). Empty when the
    /// container carries no position map at all (e.g. a KFX generated from an
    /// EPUB).
    pub fn pid_map(&self) -> HashMap<i64, i64> {
        self.axis().start_of
    }

    /// The pid axis of §10 with no Locations on it yet: where each element
    /// begins, and the mid-element anchors the position fragments state. The
    /// `{eid, pid}` shape of `position_id_map` ($265) lists them; the span
    /// shape replays each `section_position_id_map` ($609) walk for them.
    pub fn pid_axis(&self) -> PositionMap {
        self.axis().into_map(Vec::new(), None)
    }

    /// The axis as the fragments state it, before any Location divides it.
    fn axis(&self) -> Axis {
        let (axis, _) = self.pair_axis();
        if axis.start_of.is_empty() {
            return self.span_axis();
        }
        axis
    }

    /// The axis the `{eid, pid}` shape of `position_id_map` ($265) states,
    /// with the pid its closing `{eid: 0}` entry carries — the axis end, and
    /// no element of the book (§10.1). An entry carrying an `offset` ($143)
    /// re-enters an element at that character; an entry carrying none names
    /// the element's own start.
    fn pair_axis(&self) -> (Axis, Option<i64>) {
        let mut axis = Axis::default();
        let mut terminator = None;
        for frag in &self.position_id_maps {
            let Some(entries) = frag.unwrap_annotated().as_list() else {
                continue;
            };
            for entry in entries {
                let Some(fields) = entry.unwrap_annotated().as_struct() else {
                    continue;
                };
                let (Some(eid), Some(pid)) = (
                    get_field(fields, KfxSymbol::Eid as u64).and_then(|v| v.as_int()),
                    get_field(fields, KfxSymbol::Pid as u64).and_then(|v| v.as_int()),
                ) else {
                    continue;
                };
                if eid == 0 {
                    terminator = Some(pid);
                    continue;
                }
                let offset = get_field(fields, KfxSymbol::Offset as u64)
                    .and_then(|v| v.as_int())
                    .unwrap_or(0);
                axis.place(eid, offset, pid);
            }
        }

        (axis, terminator)
    }

    /// The axis the span shape of `position_id_map` ($265) states:
    /// `{contains:[{section_name, pid, length}]}`, one span per section. Map
    /// each to its base pid, then replay its `section_position_id_map` walk.
    fn span_axis(&self) -> Axis {
        let spans = self.spans();
        let mut axis = Axis::default();
        for frag in &self.section_position_id_maps {
            let walk = replay(frag);
            let base = walk
                .section
                .and_then(|s| spans.get(&s))
                .map_or(0, |(pid, _)| *pid);
            for (eid, offset, pid) in walk.assigned {
                axis.place(eid, offset, base + pid);
            }
        }

        axis
    }

    /// The `location_map` ($550) boundaries as the `{$155 id, $143 offset}`
    /// coordinates they are stated in, in source order (§10.3).
    pub fn location_anchors(&self) -> Vec<(i64, i64)> {
        let mut out = Vec::new();
        for frag in &self.location_maps {
            let Some(locs) = location_entry_list(frag) else {
                continue;
            };
            for e in locs {
                let Some(ef) = e.unwrap_annotated().as_struct() else {
                    continue;
                };
                let Some(eid) = get_field(ef, KfxSymbol::Id as u64).and_then(|v| v.as_int()) else {
                    continue;
                };
                let off = get_field(ef, KfxSymbol::Offset as u64)
                    .and_then(|v| v.as_int())
                    .unwrap_or(0);
                out.push((eid, off));
            }
        }
        out
    }

    /// The `yj.location_pid_map` ($621) boundary pids, in source order (§10.3).
    pub fn location_pids(&self) -> Vec<i64> {
        let mut out = Vec::new();
        for frag in &self.location_pid_maps {
            let Some(pids) = location_entry_list(frag) else {
                continue;
            };
            out.extend(pids.iter().filter_map(|p| p.unwrap_annotated().as_int()));
        }
        out
    }

    /// Location boundary pids, in source order. Empty when the book ships
    /// neither map, or when none of `location_map`'s anchors resolve against
    /// `axis`.
    fn location_boundaries(&self, axis: &PositionMap) -> Vec<i64> {
        let mut boundaries: Vec<i64> = self
            .location_anchors()
            .into_iter()
            .filter_map(|(eid, offset)| axis.position(eid, offset))
            .collect();
        // $621 states boundary pids directly and answers where $550 is absent;
        // §10.3 gives $550 precedence where both are present.
        if boundaries.is_empty() {
            boundaries = self.location_pids();
        }
        boundaries
    }

    /// Where the pid axis ends, one past the book's last addressable position:
    /// the largest `pid + length` the per-section spans state, or the pid the
    /// `{eid, pid}` shape's closing `{eid: 0}` entry carries.
    fn axis_extent(&self) -> Option<i64> {
        self.span_extent().or_else(|| self.pair_axis().1)
    }

    /// The axis end from `position_id_map`'s per-section spans: the largest
    /// `pid + length`, one past the book's last addressable position. `None`
    /// when the container states no spans (the `{eid, pid}` shape has none).
    fn span_extent(&self) -> Option<i64> {
        let mut end: Option<i64> = None;
        for frag in &self.position_id_maps {
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
                if let Some(pid) = get_field(ef, KfxSymbol::Pid as u64).and_then(|v| v.as_int())
                    && let Some(len) =
                        get_field(ef, KfxSymbol::Length as u64).and_then(|v| v.as_int())
                {
                    let e = pid + len.max(0);
                    end = Some(end.map_or(e, |cur: i64| cur.max(e)));
                }
            }
        }
        end
    }

    /// Assemble the whole chain into the format-agnostic scale. `None` when the
    /// container carries no position map. Positions with no location map get
    /// boundaries every `KFX_POSITIONS_PER_LOCATION` pids.
    pub fn build(&self) -> Option<PositionMap> {
        let axis = self.axis();
        if axis.start_of.is_empty() {
            return None;
        }
        let max_pid = axis.start_of.values().copied().max().unwrap_or(0);
        let undivided = self.pid_axis();
        let mut boundaries = self.location_boundaries(&undivided);
        if boundaries.is_empty() {
            let mut p = 0;
            while p <= max_pid {
                boundaries.push(p);
                p += KFX_POSITIONS_PER_LOCATION;
            }
            if boundaries.is_empty() {
                boundaries.push(0);
            }
        }
        Some(axis.into_map(boundaries, self.axis_extent()))
    }

    /// The spans `position_id_map` ($265) declares, as `section_name` ($174)
    /// symbol → `(base pid, length)`. Empty for the `{eid, pid}` shape, which
    /// names no section.
    fn spans(&self) -> HashMap<u64, (i64, Option<i64>)> {
        let mut spans = HashMap::new();
        for frag in &self.position_id_maps {
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
                    let length = get_field(ef, KfxSymbol::Length as u64).and_then(|v| v.as_int());
                    spans.insert(sym, (pid, length));
                }
            }
        }
        spans
    }

    /// Every `section_position_id_map` ($609) walk, read against the span its
    /// section holds in `position_id_map` ($265).
    pub fn section_walks(&self) -> Vec<SectionWalk> {
        let spans = self.spans();
        let mut out = Vec::new();
        for frag in &self.section_position_id_maps {
            let walk = replay(frag);
            let Some(section) = walk.section else {
                continue;
            };
            out.push(SectionWalk {
                section,
                declared_length: spans.get(&section).and_then(|(_, length)| *length),
                terminator_pid: walk.terminator_pid,
            });
        }
        out
    }
}

/// The `eid → pid` axis a container's position fragments state, gathered
/// before any Location divides it.
///
/// A `{id, offset}` coordinate (§9.4) counts characters of an element's base
/// text, and an element interrupted by a nested one runs past its own start by
/// more than its character count. The fragments state where each interrupted
/// run resumes, which [`PositionMap::position`] reads a coordinate against.
#[derive(Default)]
struct Axis {
    /// Where each element begins on the pid axis.
    start_of: HashMap<i64, i64>,
    /// Per element, the `(offset, pid)` pairs a fragment states inside it.
    anchors: HashMap<i64, Vec<(i64, i64)>>,
}

impl Axis {
    /// Record that a position fragment reached character `offset` of `eid` at
    /// `pid`. An element begins at the first entry reaching it; a later entry
    /// inside it becomes an anchor.
    fn place(&mut self, eid: i64, offset: i64, pid: i64) {
        self.start_of.entry(eid).or_insert(pid - offset);
        if offset != 0 {
            self.anchors.entry(eid).or_default().push((offset, pid));
        }
    }

    /// Hand the axis over as the format-agnostic scale, divided by
    /// `boundaries` and ending at `extent`.
    fn into_map(self, boundaries: Vec<i64>, extent: Option<i64>) -> PositionMap {
        PositionMap::new(self.start_of, boundaries, extent).with_anchors(self.anchors)
    }
}

/// One `section_position_id_map` ($609) walk, read against the span its
/// section holds in `position_id_map` ($265) (§10.2).
pub struct SectionWalk {
    /// The `section_name` ($174) symbol both fragments key on.
    pub section: u64,
    /// The `length` ($144) the section's span declares.
    pub declared_length: Option<i64>,
    /// The pid the `[advance, 0]` entry ends the walk at, counted from the
    /// section's base. `None` for a walk holding no such entry.
    pub terminator_pid: Option<i64>,
}

/// One replayed `section_position_id_map` ($609) walk.
struct Walk {
    section: Option<u64>,
    /// `(eid, offset, pid)` for each entry the walk assigns, the pid counted
    /// from the section's base and the offset counting characters into that
    /// element's base text.
    assigned: Vec<(i64, i64, i64)>,
    terminator_pid: Option<i64>,
}

/// Replay one `section_position_id_map` ($609) delta walk (§10.2). Each entry
/// advances the running pid: `[advance, eid]` names its element, `[advance, 0]`
/// ends the walk, a bare advance carries the previous element id plus one, and
/// `[advance, eid, offset]` re-enters an element at a character offset.
fn replay(frag: &IonValue) -> Walk {
    let mut walk = Walk {
        section: None,
        assigned: Vec::new(),
        terminator_pid: None,
    };
    let Some(fields) = frag.unwrap_annotated().as_struct() else {
        return walk;
    };
    walk.section = get_field(fields, KfxSymbol::SectionName as u64).and_then(sym_id);
    let Some(contains) = get_field(fields, KfxSymbol::Contains as u64).and_then(|v| v.as_list())
    else {
        return walk;
    };
    let mut pid = 0;
    let mut prev_eid: Option<i64> = None;
    for elem in contains {
        let inner = elem.unwrap_annotated();
        if let Some(entry) = inner.as_list() {
            let (Some(advance), Some(eid)) = (
                entry.first().and_then(|v| v.as_int()),
                entry.get(1).and_then(|v| v.as_int()),
            ) else {
                continue;
            };
            pid += advance;
            if eid == 0 {
                walk.terminator_pid = Some(pid);
                break;
            }
            // A third field is the character offset the entry lands on inside
            // the element.
            let offset = entry.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            walk.assigned.push((eid, offset, pid));
            prev_eid = Some(eid);
        } else if let Some(advance) = inner.as_int() {
            let Some(eid) = prev_eid.map(|e| e + 1) else {
                continue;
            };
            pid += advance;
            walk.assigned.push((eid, 0, pid));
            prev_eid = Some(eid);
        }
    }
    walk
}

/// The symbol id a value names, `None` for anything else.
fn sym_id(v: &IonValue) -> Option<u64> {
    match v.unwrap_annotated() {
        IonValue::Symbol(s) => Some(*s),
        _ => None,
    }
}

/// The `eid → pid` map of a loaded book. Shorthand for
/// [`PositionFragments::from_book`] + [`PositionFragments::pid_map`].
pub fn pid_map(book: &BookData) -> HashMap<i64, i64> {
    PositionFragments::from_book(book).pid_map()
}

/// The reading-position scale of a loaded book. Shorthand for
/// [`PositionFragments::from_book`] + [`PositionFragments::build`].
pub fn position_map(book: &BookData) -> Option<PositionMap> {
    PositionFragments::from_book(book).build()
}

/// The `$182` entry list inside a `location_map`/`yj.location_pid_map`
/// fragment, wrapped as a single-element list holding one struct: location
/// structs for $550, bare pids for $621. `None` on a shape mismatch.
fn location_entry_list(frag: &IonValue) -> Option<&[IonValue]> {
    let items = frag.unwrap_annotated().as_list()?;
    let first = items.first()?;
    let fields = first.unwrap_annotated().as_struct()?;
    get_field(fields, KfxSymbol::Locations as u64).and_then(|v| v.as_list())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::kfx::loader;

    /// §10.2. A `[advance, eid, offset]` entry re-enters an element the walk
    /// has placed. The element keeps the pid it begins at, the offset
    /// lands where the entry states, and the pids a nested element takes
    /// between them belong to neither's character count.
    #[test]
    fn a_walk_re_enters_an_element_at_a_character_offset() {
        let section = 800u64;
        let step =
            |fields: &[i64]| IonValue::List(fields.iter().copied().map(IonValue::Int).collect());
        let pid_map = IonValue::Struct(vec![(
            KfxSymbol::Contains as u64,
            IonValue::List(vec![IonValue::Struct(vec![
                (KfxSymbol::SectionName as u64, IonValue::Symbol(section)),
                (KfxSymbol::Pid as u64, IonValue::Int(100)),
                (KfxSymbol::Length as u64, IonValue::Int(12)),
            ])]),
        )]);
        let walk = IonValue::Struct(vec![
            (KfxSymbol::SectionName as u64, IonValue::Symbol(section)),
            (
                KfxSymbol::Contains as u64,
                IonValue::List(vec![
                    step(&[0, 7]),
                    step(&[4, 7, 4]),
                    IonValue::Int(0),
                    step(&[2, 7, 5]),
                    step(&[3, 9]),
                    step(&[3, 0]),
                ]),
            ),
        ]);

        let mut fragments = PositionFragments::default();
        fragments.push(KfxSymbol::PositionIdMap as u32, &pid_map);
        fragments.push(KfxSymbol::SectionPositionIdMap as u32, &walk);
        let axis = fragments.pid_axis();

        assert_eq!(axis.positions().get(&7), Some(&100));
        assert_eq!(axis.positions().get(&8), Some(&104), "the nested element");
        assert_eq!(axis.positions().get(&9), Some(&109));

        assert_eq!(axis.position(7, 0), Some(100));
        assert_eq!(axis.position(7, 3), Some(103));
        assert_eq!(axis.position(7, 4), Some(104));
        assert_eq!(axis.position(7, 5), Some(106));
        assert_eq!(axis.position(7, 6), Some(107));
        assert_eq!(axis.position(5, 0), None);

        let walks = fragments.section_walks();
        assert_eq!(walks.len(), 1);
        assert_eq!(walks[0].section, section);
        assert_eq!(walks[0].terminator_pid, Some(12));
        assert_eq!(walks[0].declared_length, Some(12));
    }

    /// §10.1. The `{eid, pid}` shape closes on an `{eid: 0}` entry, which
    /// names where the axis ends and no element of the book. An entry carrying
    /// an `offset` re-enters the element named before it.
    #[test]
    fn the_pair_shape_terminator_is_no_element() {
        let entry = |eid: i64, pid: i64, offset: Option<i64>| {
            let mut fields = vec![
                (KfxSymbol::Eid as u64, IonValue::Int(eid)),
                (KfxSymbol::Pid as u64, IonValue::Int(pid)),
            ];
            if let Some(at) = offset {
                fields.push((KfxSymbol::Offset as u64, IonValue::Int(at)));
            }
            IonValue::Struct(fields)
        };
        let pid_map = IonValue::List(vec![
            entry(4, 0, None),
            entry(5, 1, None),
            entry(4, 2, Some(1)),
            entry(0, 3, None),
        ]);
        let mut fragments = PositionFragments::default();
        fragments.push(KfxSymbol::PositionIdMap as u32, &pid_map);

        let axis = fragments.pid_axis();
        assert_eq!(axis.positions().len(), 2);
        assert_eq!(
            axis.positions().get(&4),
            Some(&0),
            "the element's own start"
        );
        assert_eq!(axis.position(4, 1), Some(2));
        assert_eq!(axis.position(0, 0), None);
        assert_eq!(fragments.axis_extent(), Some(3));
    }

    const FIXTURE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";

    /// The `eid → pid` and `eid → base text` maps come from separate walks, and
    /// an annotation slices text out of the second at coordinates from the
    /// first. Both are pinned: a change to either walk moves the digest.
    #[test]
    fn the_position_and_text_maps_are_pinned() {
        let Ok(kfx) = std::fs::read(FIXTURE) else {
            return; // fixture not present in this checkout
        };
        let book = loader::load(&kfx).expect("load fixture");
        let scale = position_map(&book).expect("fixture should carry a position map");
        let text = crate::formats::kfx::structure::eid_text_map(&book);
        let pids = pid_map(&book);

        assert!(!text.is_empty(), "fixture should carry base text");
        assert!(!pids.is_empty(), "fixture should carry positioned eids");
        assert!(scale.location_count() > 0, "fixture should carry locations");

        // The two walks share elements: at least one positioned element
        // resolves to base text.
        assert!(
            pids.keys().any(|eid| text.contains_key(eid)),
            "no positioned element carries base text"
        );

        // FNV-1a over both maps in eid order — stable across toolchains.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut fold = |bytes: &[u8]| {
            for b in bytes {
                h ^= *b as u64;
                h = h.wrapping_mul(0x1000_0000_01b3);
            }
        };
        let mut pid_entries: Vec<_> = pids.iter().collect();
        pid_entries.sort();
        for (eid, pid) in pid_entries {
            fold(format!("{eid}:{pid}\n").as_bytes());
        }
        let mut text_entries: Vec<_> = text.iter().collect();
        text_entries.sort();
        for (eid, t) in text_entries {
            fold(format!("{eid}={t}\n").as_bytes());
        }
        assert_eq!(
            h, 0x0507_e44a_494f_3911,
            "the eid→pid or eid→text map moved"
        );
    }

    /// The axis ends past the last element's start, by that element's own
    /// length. `position_id_map`'s per-section `length` is the only statement
    /// of that length, and `max(pid + length)` is the axis end.
    #[test]
    fn the_axis_ends_past_the_last_elements_start() {
        let Ok(kfx) = std::fs::read(FIXTURE) else {
            return; // fixture not present in this checkout
        };
        let book = loader::load(&kfx).expect("load fixture");
        let scale = position_map(&book).expect("fixture should carry a position map");
        let last_start = scale
            .positions()
            .values()
            .copied()
            .max()
            .expect("fixture should carry positioned eids");

        assert!(
            scale.max_position() > last_start,
            "axis ended at the last element's start ({last_start}), so the final \
             element occupies no space"
        );
        assert_eq!(scale.max_position(), 300063, "the fixture's axis end moved");
    }
}
