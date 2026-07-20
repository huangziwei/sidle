//! Reading positions: the KFX `eid → pid → device Location` chain.
//!
//! A KFX addresses text by *element id* (`eid`). The container ships two maps
//! that turn an eid into a reading position:
//!
//! - `position_id_map` ($265) — `eid → pid`, a fine-grained monotonic position.
//!   Reflowable books list `{eid, pid}` pairs directly; fixed-layout (PDF-backed)
//!   books instead describe per-section runs, so those are replayed from
//!   `section_position_id_map` ($609) to rebuild the same mapping.
//! - `location_map` ($550) / `yj.location_pid_map` ($621) — the pid boundaries
//!   dividing the book into the human "Location" numbers a Kindle displays. A
//!   book with a position map but no location map falls back to the device's
//!   own even spacing, so its Locations stay on the same scale.
//!
//! The assembled result is a [`PositionMap`], the format-agnostic scale; the
//! pid is its coordinate axis.

use std::collections::HashMap;

use crate::formats::kfx::container::get_field;
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::loader::BookData;
use crate::formats::kfx::symbols::KfxSymbol;
use crate::model::PositionMap;

/// One location boundary per this many raw pids — Amazon's own spacing when it
/// generates an approximate `location_map` (kfxlib `KFX_POSITIONS_PER_LOCATION`).
/// Used as the fallback spacing for a book that ships a position map but no
/// location map, so its Location numbers stay on the device's scale. A
/// displayed Location is ~1/110 of the raw pid; reporting the pid directly
/// inflates the number ~50× and breaks position matching against the device.
const KFX_POSITIONS_PER_LOCATION: i64 = 110;

/// The container fragments the position chain is built from, gathered by type.
///
/// Collecting them up front lets the chain be assembled either from a fully
/// loaded [`BookData`] or from a handful of entities read straight out of the
/// container — the same rules, no whole-book parse required for a caller that
/// only wants positions.
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
    /// are ignored, so a caller may hand over whatever it has.
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
        // Fixed-layout (PDF-backed) KFX: `position_id_map` is
        // `{contains:[{section_name,pid,length}]}`, not the reflowable
        // `{eid,pid}` list, so the loop above finds nothing. Rebuild `eid→pid`
        // by replaying each `section_position_id_map` walk.
        if pid_of.is_empty() {
            pid_of = self.fixed_layout_pid_map();
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

    /// Assemble the whole chain into the format-agnostic scale, or `None` when
    /// the container carries no position map — the book has no addressable
    /// reading positions to report.
    ///
    /// A book with positions but no location map gets evenly spaced boundaries
    /// at the device's own [`KFX_POSITIONS_PER_LOCATION`] interval, so its
    /// Location numbers land on the same scale as a book that ships them.
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
        Some(PositionMap::new(pid_of, boundaries))
    }

    /// `eid → pid` for a FIXED-LAYOUT (PDF-backed) KFX. There `position_id_map`
    /// ($265) is `{contains:[{section_name, pid, length}]}` (one span per
    /// section) and every section carries a `section_position_id_map` ($609)
    /// with the compact position→eid walk. Map each section to its base pid,
    /// then replay the walk.
    fn fixed_layout_pid_map(&self) -> HashMap<i64, i64> {
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
        let mine = position_map(&book).expect("fixture should carry a position map");

        let port_book = crate::kfx_to_epub::loader::load(&kfx).expect("load via port");
        let theirs = crate::kfx_to_epub::TextIndex::pid_map_from_book(&port_book);

        assert_eq!(
            mine.positions(),
            &theirs,
            "eid → pid map diverged from the port"
        );

        let max_pid = theirs.values().copied().max().unwrap_or(0);
        let port_lm = crate::kfx_to_epub::text_index::LocationMap::from_book(&port_book, &theirs)
            .unwrap_or_else(|| crate::kfx_to_epub::text_index::LocationMap::approximate(max_pid));
        assert_eq!(
            mine.location_count(),
            port_lm.count(),
            "location count diverged"
        );
        for (&eid, &pid) in &theirs {
            assert_eq!(
                mine.location_for(pid),
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
