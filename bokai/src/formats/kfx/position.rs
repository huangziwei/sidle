//! Reading positions: the KFX `eid → pid → device Location` chain.
//!
//! A KFX addresses text by *element id* (`eid`). The container ships two maps
//! that turn an eid into a reading position:
//!
//! - `position_id_map` ($265) — `eid → pid`, a fine-grained monotonic position.
//!   Reflowable books list `{eid, pid}` pairs directly; fixed-layout (PDF-backed)
//!   books instead describe per-section runs, so those are replayed to rebuild
//!   the same mapping.
//! - `location_map` ($550) / `yj.location_pid_map` ($621) — the pid boundaries
//!   dividing the book into the human "Location" numbers a Kindle displays. A
//!   book with a position map but no location map falls back to the device's
//!   own even spacing, so its Locations stay on the same scale.

use std::collections::HashMap;

use crate::formats::kfx::container::get_field;
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::loader::BookData;
use crate::formats::kfx::symbols::KfxSymbol;

/// The `eid → pid` map from `position_id_map` ($265). Empty when the container
/// carries no position map at all (e.g. a KFX generated from an EPUB).
pub fn pid_map(book: &BookData) -> HashMap<i64, i64> {
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
                    && let Some(pid) =
                        get_field(fields, KfxSymbol::Pid as u64).and_then(|v| v.as_int())
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

/// One location boundary per this many raw pids — Amazon's own spacing when it
/// generates an approximate `location_map` (kfxlib `KFX_POSITIONS_PER_LOCATION`).
/// Used as the fallback spacing for a book that ships a position map but no
/// location map, so its Location numbers stay on the device's scale.
const KFX_POSITIONS_PER_LOCATION: i64 = 110;

/// Kindle "Location" numbering for a book — the map the device uses to turn a
/// raw reading position (a `pid` from [`pid_map`]) into the
/// human "Location N" shown on screen. A location boundary sits roughly every
/// `KFX_POSITIONS_PER_LOCATION` pids, so a displayed Location is ~1/110 of the
/// raw pid; reporting the pid directly inflates the number ~50× and breaks
/// position matching against the device (the bug this fixes).
///
/// Built from the KFX `location_map` ($550) — each location is an `(eid, offset)`
/// anchor, resolved to a pid through the caller's `pid_of` — or the rarer
/// `yj.location_pid_map` ($621), which lists boundary pids directly. Both reduce
/// to the same ascending `boundaries` vector.
pub struct LocationMap {
    /// Location boundary pids, ascending. `boundaries[k]` is the pid at the
    /// start of the `(k+1)`-th location.
    boundaries: Vec<i64>,
}

impl LocationMap {
    /// Parse `$550`/`$621` into sorted boundary pids. `None` when the book ships
    /// neither map (or none of its entries resolve to a pid) — e.g.
    /// a KFX generated from an EPUB, which carries no position/location maps at all.
    pub fn from_book(book: &BookData, pid_of: &HashMap<i64, i64>) -> Option<Self> {
        let mut boundaries = Vec::new();

        // $550 location_map: [{ $182: [{ $155: eid, $143: offset }, ...] }].
        if let Some(maps) = book.by_type.get(&(KfxSymbol::LocationMap as u64)) {
            for frag in maps.values() {
                let Some(locs) = location_entry_list(frag) else {
                    continue;
                };
                for e in locs {
                    let Some(ef) = e.unwrap_annotated().as_struct() else {
                        continue;
                    };
                    let Some(eid) = get_field(ef, KfxSymbol::Id as u64).and_then(|v| v.as_int())
                    else {
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
        }

        // $621 yj.location_pid_map: [{ $182: [pid, pid, ...] }] — boundary pids
        // directly, no eid resolution needed. Only consulted when $550 is absent.
        if boundaries.is_empty()
            && let Some(maps) = book.by_type.get(&(KfxSymbol::YjLocationPidMap as u64))
        {
            for frag in maps.values() {
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

        if boundaries.is_empty() {
            return None;
        }
        boundaries.sort_unstable();
        Some(Self { boundaries })
    }

    /// Synthesize an evenly-spaced map for a book that has a real position map
    /// but no `location_map`: one boundary every `KFX_POSITIONS_PER_LOCATION`
    /// pids, matching the device's own approximate-location fallback. Keeps such
    /// books on the device's Location scale instead of showing raw pids.
    pub fn approximate(max_pid: i64) -> Self {
        let mut boundaries = Vec::new();
        let mut p = 0;
        while p <= max_pid {
            boundaries.push(p);
            p += KFX_POSITIONS_PER_LOCATION;
        }
        if boundaries.is_empty() {
            boundaries.push(0);
        }
        Self { boundaries }
    }

    /// The Kindle "Location" for a raw pid: the count of location boundaries
    /// strictly before it. A position exactly on a boundary reads as the
    /// location it *completes*, not the one it starts — the device's convention
    /// (verified: pid 128880, sitting exactly on boundary #2378, reads as
    /// "Location 2378"). Floored at 1 so the cover never shows "Loc 0".
    pub fn location_for_pid(&self, pid: i64) -> i64 {
        self.boundaries.partition_point(|&b| b < pid).max(1) as i64
    }

    /// Total number of locations — the "Loc N of M" denominator.
    pub fn count(&self) -> i64 {
        self.boundaries.len() as i64
    }
}

/// The `$182` entry list inside a `location_map`/`yj.location_pid_map` fragment,
/// which is wrapped as a single-element list holding one struct. Returns the
/// inner list (location structs for $550, bare pids for $621) or `None` on a
/// shape mismatch.
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

    /// The position chain must agree with the mechanical port's, element for
    /// element. Device annotations resolve through these numbers, so a silent
    /// divergence would mis-place every existing highlight in a book.
    #[test]
    fn matches_the_mechanical_ports_position_chain() {
        let Ok(kfx) = std::fs::read(FIXTURE) else {
            return; // fixture not present in this checkout
        };
        let book = loader::load(&kfx).expect("load fixture");
        let mine = pid_map(&book);

        let port_book = crate::kfx_to_epub::loader::load(&kfx).expect("load via port");
        let theirs = crate::kfx_to_epub::TextIndex::pid_map_from_book(&port_book);

        assert!(!mine.is_empty(), "fixture should carry a position map");
        assert_eq!(mine, theirs, "eid → pid map diverged from the port");

        let max_pid = mine.values().copied().max().unwrap_or(0);
        let lm = LocationMap::from_book(&book, &mine)
            .unwrap_or_else(|| LocationMap::approximate(max_pid));
        let port_lm = crate::kfx_to_epub::text_index::LocationMap::from_book(&port_book, &theirs)
            .unwrap_or_else(|| crate::kfx_to_epub::text_index::LocationMap::approximate(max_pid));
        assert_eq!(lm.count(), port_lm.count(), "location count diverged");
        for (&eid, &pid) in &mine {
            assert_eq!(
                lm.location_for_pid(pid),
                port_lm.location_for_pid(pid),
                "Location diverged for eid {eid} (pid {pid})"
            );
        }
    }

    /// The base-text substrate device anchors index into. Checked in BOTH
    /// directions across every positioned eid: an eid the copy *omits* would
    /// silently blank a highlight, which a one-way check would not catch.
    #[test]
    fn eid_text_matches_the_mechanical_ports_index() {
        let Ok(kfx) = std::fs::read(FIXTURE) else {
            return;
        };
        let book = loader::load(&kfx).expect("load fixture");
        let mine = crate::formats::kfx::structure::eid_text_map(&book);
        let idx = crate::kfx_to_epub::TextIndex::from_kfx(&kfx).expect("port index");

        assert!(!mine.is_empty(), "fixture should carry base text");

        // Every eid the copy produces agrees with the port.
        for (&eid, text) in &mine {
            assert_eq!(
                Some(text.as_str()),
                idx.text_of(eid),
                "base text diverged for eid {eid}"
            );
        }
        // …and over the positioned eids — the ones annotations can anchor to —
        // the port knows nothing the copy is missing.
        let mut checked = 0usize;
        for &eid in pid_map(&book).keys() {
            assert_eq!(
                mine.get(&eid).map(String::as_str),
                idx.text_of(eid),
                "base text diverged for positioned eid {eid}"
            );
            checked += 1;
        }
        assert!(checked > 0, "fixture should carry positioned eids");
    }
}
