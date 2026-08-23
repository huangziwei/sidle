//! Reading positions: the KFX `eid → pid → device Location` chain, assembled
//! from `position_id_map` ($265), `section_position_id_map` ($609) and
//! `location_map` ($550) / `yj.location_pid_map` ($621) into a [`PositionMap`].

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
        let mut pid_of = HashMap::new();
        for frag in &self.position_id_maps {
            let Some(entries) = frag.unwrap_annotated().as_list() else {
                continue;
            };
            for entry in entries {
                let Some(fields) = entry.unwrap_annotated().as_struct() else {
                    continue;
                };
                if let Some(eid) = get_field(fields, KfxSymbol::Eid as u64).and_then(|v| v.as_int())
                    && let Some(pid) =
                        get_field(fields, KfxSymbol::Pid as u64).and_then(|v| v.as_int())
                {
                    pid_of.insert(eid, pid);
                }
            }
        }
        // The span shape the loop above skips: rebuild `eid→pid` by replaying
        // each `section_position_id_map` walk.
        if pid_of.is_empty() {
            pid_of = self.section_span_pid_map();
        }
        pid_of
    }

    /// Location boundary pids, in source order. Empty when the book ships
    /// neither map, or when none of `location_map`'s anchors resolve against
    /// `pid_of`.
    fn location_boundaries(&self, pid_of: &HashMap<i64, i64>) -> Vec<i64> {
        let mut boundaries = Vec::new();

        // $550 location_map: [{ $182: [{ $155: eid, $143: offset }, ...] }].
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
                if let Some(&pid) = pid_of.get(&eid) {
                    boundaries.push(pid + off);
                }
            }
        }

        // $621 yj.location_pid_map: [{ $182: [pid, pid, ...] }] — boundary pids
        // directly, no eid resolution needed. Only consulted when $550 is absent.
        if boundaries.is_empty() {
            for frag in &self.location_pid_maps {
                let Some(pids) = location_entry_list(frag) else {
                    continue;
                };
                for p in pids {
                    if let Some(pid) = p.unwrap_annotated().as_int() {
                        boundaries.push(pid);
                    }
                }
            }
        }

        boundaries
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
        let pid_of = self.pid_map();
        if pid_of.is_empty() {
            return None;
        }
        let mut boundaries = self.location_boundaries(&pid_of);
        if boundaries.is_empty() {
            let max_pid = pid_of.values().copied().max().unwrap_or(0);
            let mut p = 0;
            while p <= max_pid {
                boundaries.push(p);
                p += KFX_POSITIONS_PER_LOCATION;
            }
            if boundaries.is_empty() {
                boundaries.push(0);
            }
        }
        Some(PositionMap::new(pid_of, boundaries, self.span_extent()))
    }

    /// `eid → pid` for the span shape of `position_id_map` ($265):
    /// `{contains:[{section_name, pid, length}]}`, one span per section. Map
    /// each to its base pid, then replay its `section_position_id_map` walk.
    fn section_span_pid_map(&self) -> HashMap<i64, i64> {
        fn sym_id(v: &IonValue) -> Option<u64> {
            match v.unwrap_annotated() {
                IonValue::Symbol(s) => Some(*s),
                _ => None,
            }
        }

        // section-name symbol → base (absolute) pid, from `position_id_map`.
        let mut base_pid: HashMap<u64, i64> = HashMap::new();
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
                    base_pid.insert(sym, pid);
                }
            }
        }

        let mut pid_of: HashMap<i64, i64> = HashMap::new();
        for frag in &self.section_position_id_maps {
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
            h, 0xddb8_e84d_0f47_e605,
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
