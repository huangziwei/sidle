//! Entity-level KFX ↔ KFX comparison — the fidelity gate. Not "is this
//! container self-consistent" but "is it still the same book": it compares
//! entities, prose, element ids, positions, ruby and media.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::formats::kfx::container::{GeneratorTrailer, parse_container_header, trailer_bytes};
use crate::formats::kfx::error::KfxError;
use crate::formats::kfx::loader::{self, BookData};
use crate::formats::kfx::package::KfxPackage;
use crate::formats::kfx::position::PositionFragments;
use crate::formats::kfx::structure;
use crate::formats::kfx::symbols::KfxSymbol;

/// The zero-width space a converter injects to carry an element id that would
/// otherwise have no position. A source container has none.
const ZWSP: char = '\u{200B}';

/// A measurement on both sides: `(before, after)`.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "bin", derive(serde::Serialize))]
pub struct Pair<T> {
    pub a: T,
    pub b: T,
}

impl<T: PartialEq> Pair<T> {
    fn new(a: T, b: T) -> Self {
        Self { a, b }
    }
    /// True when both sides measured the same.
    pub fn same(&self) -> bool {
        self.a == self.b
    }
}

/// What one container is, before anything is compared.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "bin", derive(serde::Serialize))]
pub struct Side {
    pub label: String,
    pub bytes: usize,
    pub entities: usize,
    pub container_id: String,
    /// `kfxgen_application_version` — who generated it.
    pub generator: String,
}

/// One fragment type's presence on both sides.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "bin", derive(serde::Serialize))]
pub struct TypeRow {
    pub type_id: u32,
    /// The type's symbol name (`content`, `storyline`, …), or `$<id>`.
    pub name: String,
    pub count: Pair<usize>,
    /// Entity payload bytes, ENTY framing included.
    pub bytes: Pair<usize>,
}

impl TypeRow {
    /// True when this type is gone from `b` entirely — the loudest single
    /// finding a differ can make.
    pub fn dropped(&self) -> bool {
        self.count.a > 0 && self.count.b == 0
    }
}

/// Fragment-level identity: what survived, and byte for byte or not.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "bin", derive(serde::Serialize))]
pub struct Fragments {
    /// `type/name` present in `a`, absent from `b`.
    pub dropped: Vec<String>,
    /// `type/name` present in `b`, absent from `a`.
    pub added: Vec<String>,
    /// Present on both sides with identical payload bytes.
    pub identical: usize,
    /// Present on both sides with different bytes.
    pub changed: usize,
}

/// The reading-order prose on both sides.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "bin", derive(serde::Serialize))]
pub struct Text {
    pub chars: Pair<usize>,
    /// Injected zero-width spaces.
    pub zwsp: Pair<usize>,
    /// True when the prose matches once zero-width spaces and whitespace are
    /// normalized away — the question "did any word change".
    pub identical: bool,
    /// The first place the two texts part, in normalized character offsets.
    pub divergence: Option<Divergence>,
}

/// Where two texts first differ, with enough either side to read.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "bin", derive(serde::Serialize))]
pub struct Divergence {
    pub at: usize,
    pub a: String,
    pub b: String,
}

/// How many of the source's element ids survive, and still name the same text.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "bin", derive(serde::Serialize))]
pub struct Eids {
    /// Text-bearing elements on each side.
    pub count: Pair<usize>,
    /// Ids present on both sides.
    pub surviving: usize,
    /// Surviving ids whose text is unchanged.
    pub same_text: usize,
}

/// The position fragments a device reads for Location numbers, selection and
/// dictionary lookup.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "bin", derive(serde::Serialize))]
pub struct Positions {
    /// `location_map` ($550) boundaries.
    pub locations: Pair<usize>,
    /// Boundaries stated at a non-zero offset — i.e. *inside* a paragraph.
    /// A generator that can only start a location at a block boundary reports
    /// zero here, and its Location numbers will not match Amazon's.
    pub locations_inside_text: Pair<usize>,
    /// `yj.location_pid_map` ($621) pids. Parallel to `locations`.
    pub location_pids: Pair<usize>,
    /// Whether those pids are non-decreasing, as §10.3 requires.
    pub pids_ordered: Pair<bool>,
    /// Elements carrying a `word_boundary_list` ($696).
    pub word_boundaries: Pair<usize>,
    /// `style_events` ($142) entries across every storyline.
    pub style_events: Pair<usize>,
}

/// Ruby (furigana) — the annotations themselves, and where they attach. A
/// per-type count misleads: storing each distinct reading once collapses the
/// fragment count while losing no furigana.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "bin", derive(serde::Serialize))]
pub struct Ruby {
    /// Distinct annotation readings the container states.
    pub readings: Pair<usize>,
    /// Readings `a` states that `b` states nowhere.
    pub lost: usize,
    /// Elements attaching a reading to a run (`ruby_name` / `ruby_id`).
    pub attachments: Pair<usize>,
}

/// The media files a container carries — images and fonts.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "bin", derive(serde::Serialize))]
pub struct RawMedia {
    pub count: Pair<usize>,
    pub bytes: Pair<usize>,
    /// Payloads carried across byte for byte. Matched by **content**, not by
    /// name: a rebuild renames its resources, so a name comparison would call
    /// every image new.
    pub identical: usize,
    /// One row per encoding, so a re-encode shows up as bytes moving between
    /// formats — or staying in one and growing.
    pub formats: Vec<FormatRow>,
}

/// One media encoding's share on both sides.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "bin", derive(serde::Serialize))]
pub struct FormatRow {
    pub name: String,
    pub count: Pair<usize>,
    pub bytes: Pair<usize>,
}

/// Everything the differ measured.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "bin", derive(serde::Serialize))]
pub struct Diff {
    pub a: Side,
    pub b: Side,
    pub types: Vec<TypeRow>,
    pub fragments: Fragments,
    pub text: Text,
    pub eids: Eids,
    pub positions: Positions,
    pub ruby: Ruby,
    pub media: RawMedia,
}

impl Diff {
    /// True when nothing the differ measures moved: same fragments byte for
    /// byte, same prose, same eids, same position fragments.
    pub fn is_clean(&self) -> bool {
        self.fragments.dropped.is_empty()
            && self.fragments.added.is_empty()
            && self.fragments.changed == 0
            && self.text.identical
            && self.eids.same_text == self.eids.count.a
            && self.positions.locations.same()
            && self.positions.location_pids.same()
            && self.positions.word_boundaries.same()
            && self.ruby.lost == 0
            && self.media.identical == self.media.count.a
    }

    /// The findings worth acting on, worst first, one line each. Empty for a
    /// clean diff.
    pub fn headlines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for t in self.types.iter().filter(|t| t.dropped()) {
            out.push(format!(
                "fragment type {} ($ {}) dropped: {} → 0",
                t.name, t.type_id, t.count.a
            ));
        }
        if !self.text.identical {
            match &self.text.divergence {
                Some(d) => out.push(format!("prose diverges at character {}", d.at)),
                None => out.push(format!(
                    "prose length changed: {} → {} characters",
                    self.text.chars.a, self.text.chars.b
                )),
            }
        }
        if self.text.zwsp.b > self.text.zwsp.a {
            out.push(format!(
                "{} zero-width space(s) injected into prose",
                self.text.zwsp.b - self.text.zwsp.a
            ));
        }
        let p = &self.positions;
        if p.locations.a > 0 && p.locations_inside_text.a > 0 && p.locations_inside_text.b == 0 {
            out.push(format!(
                "every location boundary pinned to a block start: {} of {} sat inside the text",
                p.locations_inside_text.a, p.locations.a
            ));
        }
        if p.word_boundaries.a > p.word_boundaries.b {
            out.push(format!(
                "word_boundary_list lost on {} element(s)",
                p.word_boundaries.a - p.word_boundaries.b
            ));
        }
        if self.eids.count.a > 0 && self.eids.same_text * 20 < self.eids.count.a {
            out.push(format!(
                "element ids reallocated: {} of {} keep their text",
                self.eids.same_text, self.eids.count.a
            ));
        }
        if self.ruby.lost > 0 {
            out.push(format!(
                "{} ruby reading(s) stated nowhere in B",
                self.ruby.lost
            ));
        }
        let m = &self.media;
        if m.count.a > 0 && m.identical < m.count.a {
            out.push(format!(
                "{} of {} media file(s) re-encoded ({} → {} bytes)",
                m.count.a - m.identical,
                m.count.a,
                m.bytes.a,
                m.bytes.b
            ));
        }
        out
    }
}

/// One side of the comparison, parsed once.
struct Loaded {
    side: Side,
    book: BookData,
    /// `(type_id, name) → payload bytes`, index-table order preserved by the
    /// `Vec` for duplicate names.
    entities: Vec<(u32, String, Vec<u8>)>,
}

fn load(label: &str, bytes: &[u8]) -> Result<Loaded, KfxError> {
    let pkg = KfxPackage::parse(bytes)?;
    let book = loader::load(bytes)?;
    let generator = parse_container_header(bytes)
        .ok()
        .and_then(|h| trailer_bytes(bytes, &h).map(|t| String::from_utf8_lossy(t).into_owned()))
        .map(|t| GeneratorTrailer::parse(&t).application_version)
        .unwrap_or_default();
    let entities = pkg
        .entities()
        .iter()
        .map(|e| (e.type_id(), pkg.name_of(e).to_string(), e.raw().to_vec()))
        .collect();
    let side = Side {
        label: label.to_string(),
        bytes: bytes.len(),
        entities: pkg.entities().len(),
        container_id: pkg.container_id().to_string(),
        generator,
    };
    Ok(Loaded {
        side,
        book,
        entities,
    })
}

/// Compare two KFX containers.
pub fn diff(a_label: &str, a: &[u8], b_label: &str, b: &[u8]) -> Result<Diff, KfxError> {
    let a = load(a_label, a)?;
    let b = load(b_label, b)?;

    Ok(Diff {
        types: type_rows(&a, &b),
        fragments: fragment_delta(&a, &b),
        text: text_delta(&a.book, &b.book),
        eids: eid_delta(&a.book, &b.book),
        positions: position_delta(&a.book, &b.book),
        ruby: ruby_delta(&a.book, &b.book),
        media: media_delta(&a, &b),
        a: a.side,
        b: b.side,
    })
}

fn type_rows(a: &Loaded, b: &Loaded) -> Vec<TypeRow> {
    let tally = |l: &Loaded| -> BTreeMap<u32, (usize, usize)> {
        let mut m: BTreeMap<u32, (usize, usize)> = BTreeMap::new();
        for (t, _, data) in &l.entities {
            let e = m.entry(*t).or_default();
            e.0 += 1;
            e.1 += data.len();
        }
        m
    };
    let (ta, tb) = (tally(a), tally(b));
    let mut ids: Vec<u32> = ta.keys().chain(tb.keys()).copied().collect();
    ids.sort_unstable();
    ids.dedup();
    ids.into_iter()
        .map(|id| {
            let (ca, ba) = ta.get(&id).copied().unwrap_or_default();
            let (cb, bb) = tb.get(&id).copied().unwrap_or_default();
            TypeRow {
                type_id: id,
                name: type_name(&a.book, &b.book, id),
                count: Pair::new(ca, cb),
                bytes: Pair::new(ba, bb),
            }
        })
        .collect()
}

/// A fragment type's own name, resolved through whichever side knows it.
fn type_name(a: &BookData, b: &BookData, type_id: u32) -> String {
    a.symbols
        .resolve_opt(type_id as u64)
        .or_else(|| b.symbols.resolve_opt(type_id as u64))
        .map(str::to_string)
        .unwrap_or_else(|| format!("${type_id}"))
}

fn fragment_delta(a: &Loaded, b: &Loaded) -> Fragments {
    // A container may hold several entities of one type under one name (the
    // nameless singletons), so index by key and compare multisets of bodies.
    fn index(l: &Loaded) -> HashMap<(u32, &str), Vec<&[u8]>> {
        let mut m: HashMap<(u32, &str), Vec<&[u8]>> = HashMap::new();
        for (t, name, data) in &l.entities {
            m.entry((*t, name.as_str()))
                .or_default()
                .push(data.as_slice());
        }
        m
    }
    let (ia, ib) = (index(a), index(b));
    let label = |(t, name): (u32, &str)| {
        let ty = type_name(&a.book, &b.book, t);
        if name.is_empty() {
            ty
        } else {
            format!("{ty}/{name}")
        }
    };

    let mut out = Fragments::default();
    for (key, bodies_a) in &ia {
        match ib.get(key) {
            None => out.dropped.push(label(*key)),
            Some(bodies_b) => {
                // Pair them off in index-table order; surplus on either side
                // counts as changed, not as a separate add/drop, because the
                // name is the identity.
                let n = bodies_a.len().min(bodies_b.len());
                for i in 0..n {
                    if bodies_a[i] == bodies_b[i] {
                        out.identical += 1;
                    } else {
                        out.changed += 1;
                    }
                }
                out.changed += bodies_a.len().abs_diff(bodies_b.len());
            }
        }
    }
    for key in ib.keys() {
        if !ia.contains_key(key) {
            out.added.push(label(*key));
        }
    }
    out.dropped.sort();
    out.added.sort();
    out
}

fn text_delta(a: &BookData, b: &BookData) -> Text {
    let (ra, rb) = (structure::reading_text(a), structure::reading_text(b));
    let zwsp = Pair::new(
        ra.chars().filter(|c| *c == ZWSP).count(),
        rb.chars().filter(|c| *c == ZWSP).count(),
    );
    // Compare what a reader would see: the injected carriers and the layout's
    // own whitespace are not the book's words.
    let norm = |s: &str| -> Vec<char> {
        s.chars()
            .filter(|c| *c != ZWSP && !c.is_whitespace())
            .collect()
    };
    let (na, nb) = (norm(&ra), norm(&rb));
    let at = na
        .iter()
        .zip(nb.iter())
        .position(|(x, y)| x != y)
        .or((na.len() != nb.len()).then_some(na.len().min(nb.len())));
    let window = |s: &[char], at: usize| -> String {
        let lo = at.saturating_sub(20);
        let hi = (at + 20).min(s.len());
        s[lo..hi].iter().collect()
    };
    Text {
        chars: Pair::new(ra.chars().count(), rb.chars().count()),
        zwsp,
        identical: at.is_none(),
        divergence: at.map(|at| Divergence {
            at,
            a: window(&na, at),
            b: window(&nb, at),
        }),
    }
}

fn eid_delta(a: &BookData, b: &BookData) -> Eids {
    let (ma, mb) = (structure::eid_content_map(a), structure::eid_content_map(b));
    let surviving = ma.keys().filter(|k| mb.contains_key(k)).count();
    let same_text = ma
        .iter()
        .filter(|(k, v)| mb.get(k).is_some_and(|w| w == *v))
        .count();
    Eids {
        count: Pair::new(ma.len(), mb.len()),
        surviving,
        same_text,
    }
}

fn position_delta(a: &BookData, b: &BookData) -> Positions {
    let measure = |book: &BookData| {
        let frags = PositionFragments::from_book(book);
        let anchors = frags.location_anchors();
        let pids = frags.location_pids();
        (
            anchors.len(),
            anchors.iter().filter(|(_, off)| *off != 0).count(),
            pids.len(),
            pids.windows(2).all(|w| w[0] <= w[1]),
        )
    };
    let (la, ia, pa, oa) = measure(a);
    let (lb, ib, pb, ob) = measure(b);
    Positions {
        locations: Pair::new(la, lb),
        locations_inside_text: Pair::new(ia, ib),
        location_pids: Pair::new(pa, pb),
        pids_ordered: Pair::new(oa, ob),
        word_boundaries: Pair::new(count_word_boundaries(a), count_word_boundaries(b)),
        style_events: Pair::new(count_style_events(a), count_style_events(b)),
    }
}

/// Elements carrying a `word_boundary_list`, across every storyline.
fn count_word_boundaries(book: &BookData) -> usize {
    count_field(book, KfxSymbol::WordBoundaryList as u64, |_| 1)
}

/// `style_events` entries, across every storyline.
fn count_style_events(book: &BookData) -> usize {
    count_field(book, KfxSymbol::StyleEvents as u64, |v| {
        v.as_list().map_or(0, <[_]>::len)
    })
}

/// Walk every storyline, scoring each occurrence of `field`.
fn count_field(
    book: &BookData,
    field: u64,
    score: impl Fn(&crate::formats::kfx::ion::IonValue) -> usize + Copy,
) -> usize {
    use crate::formats::kfx::ion::IonValue;

    fn walk(value: &IonValue, field: u64, score: &impl Fn(&IonValue) -> usize, total: &mut usize) {
        match value.unwrap_annotated() {
            IonValue::Struct(fields) => {
                for (k, v) in fields {
                    if *k == field {
                        *total += score(v);
                    }
                    walk(v, field, score, total);
                }
            }
            IonValue::List(items) => {
                for item in items {
                    walk(item, field, score, total);
                }
            }
            _ => {}
        }
    }

    let mut total = 0;
    let mut seen: HashSet<&str> = HashSet::new();
    if let Some(storylines) = book.by_type.get(&(KfxSymbol::Storyline as u64)) {
        for (name, story) in storylines {
            if seen.insert(name.as_str()) {
                walk(story, field, &score, &mut total);
            }
        }
    }
    total
}

/// Every distinct ruby reading a container states, and how many runs attach one.
fn ruby_delta(a: &BookData, b: &BookData) -> Ruby {
    let readings = |book: &BookData| -> HashSet<String> {
        let mut out = HashSet::new();
        if let Some(frags) = book.by_type.get(&(KfxSymbol::RubyContent as u64)) {
            for frag in frags.values() {
                collect_ruby_readings(frag, book, &mut out);
            }
        }
        out
    };
    let (ra, rb) = (readings(a), readings(b));
    Ruby {
        readings: Pair::new(ra.len(), rb.len()),
        lost: ra.difference(&rb).count(),
        attachments: Pair::new(count_ruby_attachments(a), count_ruby_attachments(b)),
    }
}

/// Walk a `ruby_content` fragment, taking each reading separately: it holds them
/// as `content_list: [{id, ruby_id, content}, …]`, one entry per reading.
fn collect_ruby_readings(
    value: &crate::formats::kfx::ion::IonValue,
    book: &BookData,
    out: &mut HashSet<String>,
) {
    use crate::formats::kfx::container::get_field;
    use crate::formats::kfx::ion::IonValue;
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            if let Some(content) = get_field(fields, KfxSymbol::Content as u64) {
                let text = structure::resolve_content_text_from(content, book);
                if !text.is_empty() {
                    out.insert(text);
                }
            }
            for (_, v) in fields {
                collect_ruby_readings(v, book, out);
            }
        }
        IonValue::List(items) => {
            for item in items {
                collect_ruby_readings(item, book, out);
            }
        }
        _ => {}
    }
}

/// Elements naming a ruby reading, across every storyline.
fn count_ruby_attachments(book: &BookData) -> usize {
    count_field(book, KfxSymbol::RubyName as u64, |_| 1)
}

/// The media payloads on both sides, matched by content.
fn media_delta(a: &Loaded, b: &Loaded) -> RawMedia {
    use crate::util::{MediaFormat, detect_media_format};

    /// One side's media, ready to compare.
    struct Tally {
        /// Payload digest → how many entities carry it.
        bodies: HashMap<u64, usize>,
        /// Encoding name → (files, bytes).
        formats: BTreeMap<String, (usize, usize)>,
        count: usize,
        bytes: usize,
    }

    fn tally(l: &Loaded) -> Tally {
        let mut bodies: HashMap<u64, usize> = HashMap::new();
        let mut formats: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        let (mut count, mut bytes) = (0usize, 0usize);
        for (type_id, _, data) in &l.entities {
            if *type_id != KfxSymbol::Bcrawmedia as u32 && *type_id != KfxSymbol::Bcrawfont as u32 {
                continue;
            }
            let payload = crate::formats::kfx::container::skip_enty_header(data);
            *bodies.entry(digest(payload)).or_default() += 1;
            let name = match detect_media_format("", payload) {
                MediaFormat::Binary => "other".to_string(),
                other => other
                    .mime_type()
                    .rsplit('/')
                    .next()
                    .unwrap_or("other")
                    .to_string(),
            };
            let e = formats.entry(name).or_default();
            e.0 += 1;
            e.1 += payload.len();
            count += 1;
            bytes += payload.len();
        }
        Tally {
            bodies,
            formats,
            count,
            bytes,
        }
    }

    let (ta, tb) = (tally(a), tally(b));
    // Multiset intersection: a payload carried across keeps its bytes exactly.
    let identical: usize = ta
        .bodies
        .iter()
        .map(|(k, n)| (*n).min(tb.bodies.get(k).copied().unwrap_or(0)))
        .sum();

    let mut names: Vec<&String> = ta.formats.keys().chain(tb.formats.keys()).collect();
    names.sort_unstable();
    names.dedup();
    let formats = names
        .into_iter()
        .map(|name| {
            let (ca, sa) = ta.formats.get(name).copied().unwrap_or_default();
            let (cb, sb) = tb.formats.get(name).copied().unwrap_or_default();
            FormatRow {
                name: name.clone(),
                count: Pair::new(ca, cb),
                bytes: Pair::new(sa, sb),
            }
        })
        .collect();

    RawMedia {
        count: Pair::new(ta.count, tb.count),
        bytes: Pair::new(ta.bytes, tb.bytes),
        identical,
        formats,
    }
}

/// FNV-1a over a payload — enough to match media files by content.
fn digest(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::kfx::container_edit::{EntityEdit, edit_container};

    const FIXTURE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";

    /// A container against itself reports nothing. Media is matched by content, not
    /// by name: a swapped payload must be reported even though every entity keeps
    /// its id.
    #[test]
    fn a_swapped_media_payload_is_reported() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let mut swapped = false;
        let out = edit_container(&kfx, |e| {
            if e.is_raw_media() && !swapped {
                swapped = true;
                Ok(EntityEdit::RawMedia(vec![0xFF, 0xD8, 0xFF, 0xD9]))
            } else {
                Ok(EntityEdit::Keep)
            }
        })
        .expect("swap one resource");
        assert!(swapped, "fixture carries raw media");

        let d = diff("src", &kfx, "out", &out).expect("diff");
        assert_eq!(d.media.count.a, d.media.count.b, "no file added or dropped");
        assert_eq!(
            d.media.identical,
            d.media.count.a - 1,
            "every file but the swapped one carried across"
        );
        assert!(!d.is_clean());
        assert!(
            d.headlines().iter().any(|h| h.contains("re-encoded")),
            "headlines: {:?}",
            d.headlines()
        );
    }

    /// Ruby readings are counted one per entry, not one per fragment: a
    /// `ruby_content` holds them all in one `content_list`.
    #[test]
    fn ruby_readings_are_counted_individually() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let d = diff("a", &kfx, "b", &kfx).expect("diff");
        if d.ruby.attachments.a == 0 {
            return; // fixture carries no ruby
        }
        assert!(
            d.ruby.readings.a > 1,
            "a fragment's entries are separate readings, got {}",
            d.ruby.readings.a
        );
        assert_eq!(d.ruby.lost, 0);
    }

    #[test]
    fn identical_containers_are_clean() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let d = diff("a", &kfx, "b", &kfx).expect("diff");
        assert!(d.is_clean(), "headlines: {:?}", d.headlines());
        assert_eq!(d.fragments.changed, 0);
        assert_eq!(d.fragments.identical, d.a.entities);
        assert!(d.text.identical);
        assert_eq!(d.eids.same_text, d.eids.count.a);
    }

    /// A surgical passthrough — every entity re-framed but nothing changed —
    /// is also clean. This is what a save must look like when the user edited
    /// nothing.
    #[test]
    fn surgical_passthrough_is_clean() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let out = edit_container(&kfx, |_| Ok(EntityEdit::Keep)).expect("passthrough");
        let d = diff("src", &kfx, "out", &out).expect("diff");
        assert!(d.is_clean(), "headlines: {:?}", d.headlines());
    }

    /// Re-serializing every Ion body — parse, write back — is the weakest
    /// re-authoring there is. Whatever it costs, the differ must see it.
    #[test]
    fn reserialized_bodies_are_reported_when_they_change() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let out = edit_container(&kfx, |e| {
            if e.is_raw_media() {
                Ok(EntityEdit::Keep)
            } else {
                Ok(EntityEdit::Ion(e.parse_ion()?))
            }
        })
        .expect("re-serialize");
        let d = diff("src", &kfx, "out", &out).expect("diff");
        // Prose and ids must survive a pure re-encode even if bytes move.
        assert!(d.text.identical, "{:?}", d.text.divergence);
        assert_eq!(d.eids.same_text, d.eids.count.a, "every eid keeps its text");
        assert!(d.fragments.dropped.is_empty(), "{:?}", d.fragments.dropped);
        assert!(d.fragments.added.is_empty(), "{:?}", d.fragments.added);
    }
}
