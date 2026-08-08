//! Kindle Reader Data Store — the container behind a book's `.sdr` sidecars.
//!
//! A Kindle keeps its per-book reading state beside the book: `<book>.sdr/`
//! holds a `.yjr` (annotations, font choices, synced position) and a `.yjf`
//! (whatever changes every page turn — last page read, reading timers). AZW3-era
//! devices name the same two files `.azw3r` / `.azw3f`. All four are this
//! format, which is Amazon's own and unrelated to Ion despite living next to
//! KFX.
//!
//! The container is a tree of **named objects** holding **typed values**:
//!
//! ```text
//! signature  00 00 00 00 00 1A B1 26
//! version    a long
//! count      an int — how many top-level objects follow
//! objects    FE <name> <value>* FF, nesting freely
//! ```
//!
//! Values are a type byte then a big-endian payload; an object's *name* is a
//! bare UTF-8 payload with no type byte of its own. There is no checksum and no
//! signature beyond the magic, which is what makes a host-authored sidecar
//! something a device will accept.
//!
//! ## Why this round-trips exactly
//!
//! [`Store::encode`] reproduces its input byte-for-byte, and the tests hold it
//! to that over real device files. This is a hard requirement rather than a
//! nicety: writing a sidecar back means handing a file to firmware that also
//! keeps `font.prefs`, `ReaderMetrics` and other records this crate has no
//! opinion about. Preserving unknown records verbatim is the difference between
//! adding a highlight and quietly resetting someone's reader.

use std::fmt;

/// Magic bytes every sidecar opens with.
pub const SIGNATURE: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x1A, 0xB1, 0x26];

const T_BOOL: u8 = 0;
const T_INT: u8 = 1;
const T_LONG: u8 = 2;
const T_UTF: u8 = 3;
const T_DOUBLE: u8 = 4;
const T_SHORT: u8 = 5;
const T_FLOAT: u8 = 6;
const T_BYTE: u8 = 7;
const T_CHAR: u8 = 9;
const T_OBJECT_BEGIN: u8 = 0xFE;
const T_OBJECT_END: u8 = 0xFF;

/// The object holding a book's annotations, keyed by kind.
const ANNOTATION_CACHE: &str = "annotation.cache.object";
/// One kind's annotation list inside [`ANNOTATION_CACHE`].
const INTERVAL_TREE: &str = "saved.avl.interval.tree";
/// Prefix every annotation record's object name carries.
const PERSONAL: &str = "annotation.personal.";

/// What went wrong reading a sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KrdsError {
    /// The file doesn't open with [`SIGNATURE`].
    NotASidecar,
    /// A value ran past the end of the buffer.
    Truncated { at: usize },
    /// A type byte this format doesn't define.
    UnknownType { byte: u8, at: usize },
    /// A string that isn't valid UTF-8.
    BadUtf8 { at: usize },
    /// Bytes left over after the declared object count was read.
    TrailingBytes { at: usize },
}

impl fmt::Display for KrdsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotASidecar => write!(f, "not a KRDS sidecar (bad signature)"),
            Self::Truncated { at } => write!(f, "value runs past end of buffer at {at}"),
            Self::UnknownType { byte, at } => write!(f, "unknown type byte {byte:#04x} at {at}"),
            Self::BadUtf8 { at } => write!(f, "invalid UTF-8 string at {at}"),
            Self::TrailingBytes { at } => write!(f, "trailing bytes after last object at {at}"),
        }
    }
}

impl std::error::Error for KrdsError {}

/// One typed value. `Double`/`Float` keep the source's exact bit pattern, so a
/// value this crate never interprets still re-encodes unchanged.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(u8),
    Int(i32),
    Long(i64),
    /// `None` is the format's null string, which carries no length or payload.
    Utf8(Option<String>),
    Double([u8; 8]),
    Short(i16),
    Float([u8; 4]),
    Byte(u8),
    Char(u8),
    Object(Object),
}

impl Value {
    /// The string in a `Utf8` value, if this is one and it isn't null.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Utf8(Some(s)) => Some(s),
            _ => None,
        }
    }

    /// The number in an `Int`, widened.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(v) => Some(i64::from(*v)),
            _ => None,
        }
    }

    /// The number in a `Long`.
    pub fn as_long(&self) -> Option<i64> {
        match self {
            Self::Long(v) => Some(*v),
            _ => None,
        }
    }

    /// This value if it is an object.
    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Self::Object(o) => Some(o),
            _ => None,
        }
    }
}

/// A named node: a record type, and the values that make it up.
#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    pub name: String,
    pub values: Vec<Value>,
}

impl Object {
    /// An empty object with this name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            values: Vec::new(),
        }
    }

    /// The first direct child object called `name`.
    pub fn child(&self, name: &str) -> Option<&Object> {
        self.values
            .iter()
            .filter_map(Value::as_object)
            .find(|o| o.name == name)
    }

    /// Every object at or below this one, self first, in document order.
    fn walk<'a>(&'a self, out: &mut Vec<&'a Object>) {
        out.push(self);
        for v in &self.values {
            if let Value::Object(o) = v {
                o.walk(out);
            }
        }
    }
}

/// A parsed sidecar.
#[derive(Debug, Clone, PartialEq)]
pub struct Store {
    /// Format version from the header; carried so [`Self::encode`] restores it.
    pub version: i64,
    pub roots: Vec<Object>,
}

// ---------------------------------------------------------------------------
// Container codec
// ---------------------------------------------------------------------------

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], KrdsError> {
        let end = self
            .i
            .checked_add(n)
            .ok_or(KrdsError::Truncated { at: self.i })?;
        let s = self
            .b
            .get(self.i..end)
            .ok_or(KrdsError::Truncated { at: self.i })?;
        self.i = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, KrdsError> {
        Ok(self.take(1)?[0])
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    /// A UTF-8 payload without its type byte: null flag, then length + bytes.
    fn utf_body(&mut self) -> Result<Option<String>, KrdsError> {
        if self.u8()? != 0 {
            return Ok(None);
        }
        let at = self.i;
        let n = u16::from_be_bytes(self.take(2)?.try_into().expect("2 bytes")) as usize;
        let raw = self.take(n)?;
        String::from_utf8(raw.to_vec())
            .map(Some)
            .map_err(|_| KrdsError::BadUtf8 { at })
    }

    fn value(&mut self) -> Result<Value, KrdsError> {
        let at = self.i;
        let t = self.u8()?;
        Ok(match t {
            T_BOOL => Value::Bool(self.u8()?),
            T_INT => Value::Int(i32::from_be_bytes(self.take(4)?.try_into().expect("4"))),
            T_LONG => Value::Long(i64::from_be_bytes(self.take(8)?.try_into().expect("8"))),
            T_UTF => Value::Utf8(self.utf_body()?),
            T_DOUBLE => Value::Double(self.take(8)?.try_into().expect("8")),
            T_SHORT => Value::Short(i16::from_be_bytes(self.take(2)?.try_into().expect("2"))),
            T_FLOAT => Value::Float(self.take(4)?.try_into().expect("4")),
            T_BYTE => Value::Byte(self.u8()?),
            T_CHAR => Value::Char(self.u8()?),
            T_OBJECT_BEGIN => Value::Object(self.object()?),
            other => return Err(KrdsError::UnknownType { byte: other, at }),
        })
    }

    /// Reads an object's body; the `0xFE` that opened it is already consumed.
    fn object(&mut self) -> Result<Object, KrdsError> {
        let name = self.utf_body()?.unwrap_or_default();
        let mut values = Vec::new();
        while self.peek() != Some(T_OBJECT_END) {
            values.push(self.value()?);
        }
        self.u8()?; // the terminator
        Ok(Object { name, values })
    }
}

fn write_utf_body(out: &mut Vec<u8>, s: Option<&str>) {
    match s {
        None => out.push(1),
        Some(s) => {
            out.push(0);
            out.extend_from_slice(&(s.len() as u16).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
    }
}

fn write_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Bool(b) => out.extend_from_slice(&[T_BOOL, *b]),
        Value::Int(n) => {
            out.push(T_INT);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Value::Long(n) => {
            out.push(T_LONG);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Value::Utf8(s) => {
            out.push(T_UTF);
            write_utf_body(out, s.as_deref());
        }
        Value::Double(raw) => {
            out.push(T_DOUBLE);
            out.extend_from_slice(raw);
        }
        Value::Short(n) => {
            out.push(T_SHORT);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Value::Float(raw) => {
            out.push(T_FLOAT);
            out.extend_from_slice(raw);
        }
        Value::Byte(b) => out.extend_from_slice(&[T_BYTE, *b]),
        Value::Char(c) => out.extend_from_slice(&[T_CHAR, *c]),
        Value::Object(o) => write_object(out, o),
    }
}

fn write_object(out: &mut Vec<u8>, o: &Object) {
    out.push(T_OBJECT_BEGIN);
    write_utf_body(out, Some(&o.name));
    for v in &o.values {
        write_value(out, v);
    }
    out.push(T_OBJECT_END);
}

impl Store {
    /// Read a sidecar.
    pub fn parse(bytes: &[u8]) -> Result<Self, KrdsError> {
        let mut r = Reader { b: bytes, i: 0 };
        if r.take(8)? != SIGNATURE {
            return Err(KrdsError::NotASidecar);
        }
        let version = r.value()?.as_long().unwrap_or(1);
        let count = r.value()?.as_int().unwrap_or(0).max(0) as usize;
        let mut roots = Vec::with_capacity(count);
        for _ in 0..count {
            if r.u8()? != T_OBJECT_BEGIN {
                return Err(KrdsError::UnknownType {
                    byte: r.b.get(r.i - 1).copied().unwrap_or(0),
                    at: r.i - 1,
                });
            }
            roots.push(r.object()?);
        }
        if r.i != bytes.len() {
            return Err(KrdsError::TrailingBytes { at: r.i });
        }
        Ok(Self { version, roots })
    }

    /// Write it back. Byte-identical to the input when nothing was changed.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&SIGNATURE);
        write_value(&mut out, &Value::Long(self.version));
        write_value(&mut out, &Value::Int(self.roots.len() as i32));
        for o in &self.roots {
            write_object(&mut out, o);
        }
        out
    }

    /// A minimal, valid sidecar: an empty annotation cache and nothing else.
    ///
    /// What to start from for a book that has no sidecar yet. A device fills in
    /// its own `font.prefs` and friends the first time it opens the book; the
    /// point here is to carry annotations, not to invent reader preferences.
    pub fn empty() -> Self {
        Self {
            version: 1,
            roots: vec![Object::new(ANNOTATION_CACHE)],
        }
    }

    /// The first top-level object called `name`.
    pub fn root(&self, name: &str) -> Option<&Object> {
        self.roots.iter().find(|o| o.name == name)
    }

    /// Every object in the file, in document order.
    fn all_objects(&self) -> Vec<&Object> {
        let mut out = Vec::new();
        for r in &self.roots {
            r.walk(&mut out);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Reader-data vocabulary: anchors, annotations, positions
// ---------------------------------------------------------------------------

/// Which `annotation.personal.*` record this is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Highlight,
    Note,
    Bookmark,
    /// Handwriting drawn on a sideloaded document. One record per drawn page:
    /// the anchor is the host page, the inline body the ink notebook's
    /// page-container id. It covers no text.
    Handwritten,
    /// Any other `annotation.personal.<x>` — kept verbatim rather than dropped.
    Other(String),
}

impl Kind {
    /// Map an `annotation.personal.<suffix>` segment to a kind. The same three
    /// words appear as the `Your <Kind>` line in `My Clippings.txt`, so a
    /// clippings parser can share this.
    pub fn parse(suffix: &str) -> Self {
        match suffix {
            "highlight" => Self::Highlight,
            "note" => Self::Note,
            "bookmark" => Self::Bookmark,
            "handwritten_note" => Self::Handwritten,
            other => Self::Other(other.to_string()),
        }
    }

    /// The lowercase tag, as it appears after `annotation.personal.`.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Highlight => "highlight",
            Self::Note => "note",
            Self::Bookmark => "bookmark",
            Self::Handwritten => "handwritten_note",
            Self::Other(s) => s,
        }
    }

    /// This kind's object name inside the file.
    pub fn record_name(&self) -> String {
        format!("{PERSONAL}{}", self.as_str())
    }

    /// The int key this kind occupies in [`ANNOTATION_CACHE`]'s map. `None` for
    /// a kind whose slot this crate hasn't seen a device use.
    pub fn cache_key(&self) -> Option<i32> {
        Some(match self {
            Self::Bookmark => 0,
            Self::Highlight => 1,
            Self::Note => 2,
            Self::Handwritten => 10,
            Self::Other(_) => return None,
        })
    }
}

/// One endpoint of an annotation: which element, how far into it, and where
/// that lands on the book's linear scale.
///
/// On the wire this is `base64(type, eid, offset) ":" position` — the base64 is
/// the authoritative anchor and the trailing number a derived reading position
/// (what a device shows as a "Location"). Both are reproduced exactly, so a
/// decoded anchor re-encodes to the string it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    /// Leading byte of the packed form; `1` in everything observed.
    pub type_byte: u8,
    /// Source element id.
    pub eid: i64,
    /// Character offset within that element.
    pub offset: i64,
    /// Linear position on the book's own scale.
    pub position: i64,
}

impl Anchor {
    /// A fresh anchor in the shape devices write.
    pub fn new(eid: i64, offset: i64, position: i64) -> Self {
        Self {
            type_byte: 1,
            eid,
            offset,
            position,
        }
    }

    /// Decode `base64(type, eid, offset):position`. `None` for anything that
    /// isn't one — a note body, or the placeholder standing in for covered text.
    pub fn decode(s: &str) -> Option<Self> {
        let (b64, pos) = s.rsplit_once(':')?;
        let position: i64 = pos.parse().ok()?;
        let raw = base64_decode(b64)?;
        if raw.len() != 9 {
            return None;
        }
        Some(Self {
            type_byte: raw[0],
            eid: i64::from(u32::from_le_bytes([raw[1], raw[2], raw[3], raw[4]])),
            offset: i64::from(u32::from_le_bytes([raw[5], raw[6], raw[7], raw[8]])),
            position,
        })
    }

    /// The wire form.
    pub fn encode(&self) -> String {
        let mut raw = [0u8; 9];
        raw[0] = self.type_byte;
        raw[1..5].copy_from_slice(&(self.eid as u32).to_le_bytes());
        raw[5..9].copy_from_slice(&(self.offset as u32).to_le_bytes());
        format!("{}:{}", base64_encode(&raw), self.position)
    }
}

/// `U+FFFC OBJECT REPLACEMENT CHARACTER` — the device's stand-in for the text an
/// annotation covers, which the file never stores.
pub const TEXT_PLACEHOLDER: char = '\u{FFFC}';

/// The `template` value devices write on a plain highlight.
const DEFAULT_TEMPLATE: &str = "0\u{FFFC}0";

/// The highlight colours a Kindle names, as it spells them.
///
/// A colour-capable device (Colorsoft) appends the name as a plain string after
/// the template; a monochrome one writes no colour value at all, which is why
/// absence means "the device had nothing to say", not "yellow".
pub const COLORS: [&str; 4] = ["yellow", "blue", "pink", "orange"];

/// Whether a trailing string is a colour rather than a note body.
///
/// The two occupy the same shape — an untyped string after the template — so
/// they are told apart by value. A note whose body is exactly a colour word
/// would be read as a colour; that is the known cost of a format that doesn't
/// label its fields.
fn is_color(s: &str) -> bool {
    COLORS.contains(&s)
}

/// One annotation record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub kind: Kind,
    /// `[start, end]` for a span; a bookmark repeats its point. Empty only for a
    /// malformed record with no decodable anchor.
    pub anchors: Vec<Anchor>,
    /// A note's typed body, or a handwritten record's page-container id — the
    /// inline string that isn't an anchor. Absent on a plain highlight.
    pub body: Option<String>,
    /// Highlight colour, one of [`COLORS`]. `None` from a monochrome device,
    /// which writes no colour at all rather than naming a default.
    pub color: Option<String>,
    /// Creation / last-modification stamps, epoch milliseconds.
    pub created_ms: Option<i64>,
    pub modified_ms: Option<i64>,
}

impl Annotation {
    /// A highlight over `[start, end]`, shaped the way a device writes one.
    /// Pass `color` only for a colour-capable device; `None` reproduces what a
    /// monochrome Kindle writes.
    pub fn highlight(start: Anchor, end: Anchor, created_ms: i64, color: Option<&str>) -> Self {
        Self {
            kind: Kind::Highlight,
            anchors: vec![start, end],
            body: None,
            color: color.map(str::to_string),
            created_ms: Some(created_ms),
            modified_ms: Some(created_ms),
        }
    }

    /// Start anchor.
    pub fn start(&self) -> Option<&Anchor> {
        self.anchors.first()
    }

    /// End anchor; a bookmark repeats its start, so fall back to it.
    pub fn end(&self) -> Option<&Anchor> {
        self.anchors.get(1).or_else(|| self.anchors.first())
    }

    /// Whether two records cover the same span — the identity a merge dedups on,
    /// since the file carries nothing else to tell them apart.
    pub fn same_span(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.start().map(|a| (a.eid, a.offset)) == other.start().map(|a| (a.eid, a.offset))
            && self.end().map(|a| (a.eid, a.offset)) == other.end().map(|a| (a.eid, a.offset))
    }

    fn from_object(o: &Object) -> Option<Self> {
        let suffix = o.name.strip_prefix(PERSONAL)?;
        let mut anchors = Vec::new();
        let mut body = None;
        let mut color = None;
        let mut stamps = Vec::new();
        for v in &o.values {
            match v {
                Value::Utf8(Some(s)) => match Anchor::decode(s) {
                    Some(a) => anchors.push(a),
                    None if is_color(s) => color = Some(s.clone()),
                    // The placeholder is not a body; a real note or container id is.
                    None if !s.contains(TEXT_PLACEHOLDER) && !s.trim().is_empty() => {
                        body = Some(s.clone())
                    }
                    None => {}
                },
                Value::Long(ms) => stamps.push(*ms),
                _ => {}
            }
        }
        Some(Self {
            kind: Kind::parse(suffix),
            anchors,
            body,
            color,
            created_ms: stamps.first().copied(),
            modified_ms: stamps.get(1).copied(),
        })
    }

    fn to_object(&self) -> Object {
        let mut values = vec![
            Value::Utf8(self.start().map(Anchor::encode)),
            Value::Utf8(self.end().map(Anchor::encode)),
            Value::Long(self.created_ms.unwrap_or(0)),
            Value::Long(self.modified_ms.or(self.created_ms).unwrap_or(0)),
            Value::Utf8(Some(DEFAULT_TEMPLATE.to_string())),
        ];
        // Body then colour. A colour-only highlight reproduces exactly what a
        // Colorsoft writes; the order when a note carries both is inferred, not
        // observed — no coloured note has been seen on a device yet.
        if let Some(body) = &self.body {
            values.push(Value::Utf8(Some(body.clone())));
        }
        if let Some(color) = &self.color {
            values.push(Value::Utf8(Some(color.clone())));
        }
        Object {
            name: self.kind.record_name(),
            values,
        }
    }
}

impl Store {
    /// Every annotation in the file, in document order.
    ///
    /// Records are found by name anywhere in the tree rather than by walking the
    /// map's declared shape, so a sidecar whose cache is laid out differently
    /// than expected still yields its annotations.
    pub fn annotations(&self) -> Vec<Annotation> {
        self.all_objects()
            .into_iter()
            .filter_map(Annotation::from_object)
            .collect()
    }

    /// A named position — `lpr` (last page read) or `fpr` — from a `.yjf`.
    ///
    /// The record holds the same anchor form an annotation uses, sometimes
    /// behind a version byte, so the first decodable string wins.
    pub fn position(&self, key: &str) -> Option<Anchor> {
        let o = self.all_objects().into_iter().find(|o| o.name == key)?;
        o.values
            .iter()
            .filter_map(Value::as_str)
            .find_map(Anchor::decode)
    }

    /// When a named position record was last written, in epoch milliseconds.
    ///
    /// Both `lpr` and `fpr` carry their timestamp as the first long in the
    /// record. Across a library this orders the books by when each was last
    /// read, which is how a caller works out which book a device currently has
    /// open — the one whose reader state is live in memory.
    pub fn position_time(&self, key: &str) -> Option<i64> {
        let o = self.all_objects().into_iter().find(|o| o.name == key)?;
        o.values
            .iter()
            .find_map(Value::as_long)
            .filter(|ms| *ms > 0)
    }

    /// Add `incoming` to the annotation cache, leaving every other record in the
    /// file untouched. Returns how many were new.
    ///
    /// A record whose span already exists is skipped — the device's own copy
    /// stays, keeping its stamps. New records join their kind's list and the
    /// list is re-sorted by start position, which is the order devices write and
    /// the order the "interval tree" name implies. Kinds already in the file
    /// that aren't being added to are not rewritten at all.
    pub fn merge_annotations(&mut self, incoming: &[Annotation]) -> usize {
        if incoming.is_empty() {
            return 0;
        }
        let cache = self.cache_object_mut();
        let mut slots = read_slots(cache);
        let mut added = 0;

        for record in incoming {
            let Some(key) = record.kind.cache_key() else {
                continue; // no known slot — refuse to guess where it goes
            };
            let slot = match slots.iter_mut().find(|(k, _)| *k == key) {
                Some((_, list)) => list,
                None => {
                    slots.push((key, Vec::new()));
                    &mut slots.last_mut().expect("just pushed").1
                }
            };
            if slot.iter().any(|existing| existing.same_span(record)) {
                continue;
            }
            slot.push(record.clone());
            added += 1;
        }

        if added > 0 {
            for (_, list) in &mut slots {
                list.sort_by_key(|a| a.start().map_or(i64::MAX, |s| s.position));
            }
            slots.sort_by_key(|(k, _)| *k);
            write_slots(cache, &slots);
        }
        added
    }

    /// The annotation cache, created empty if the file has none.
    fn cache_object_mut(&mut self) -> &mut Object {
        if let Some(i) = self.roots.iter().position(|o| o.name == ANNOTATION_CACHE) {
            return &mut self.roots[i];
        }
        // Devices put it after `next.in.series.info.data` and before
        // `ReaderMetrics`; position among roots carries no meaning, so append.
        self.roots.push(Object::new(ANNOTATION_CACHE));
        self.roots.last_mut().expect("just pushed")
    }
}

/// Read the cache's `int size, then size × (int key, tree object)` layout.
/// A shape that doesn't match yields nothing rather than a partial read — the
/// caller rewrites the whole cache, so a misread would silently drop records.
fn read_slots(cache: &Object) -> Vec<(i32, Vec<Annotation>)> {
    let mut out: Vec<(i32, Vec<Annotation>)> = Vec::new();
    let mut it = cache.values.iter();
    // Leading size int; an empty cache is written with no size at all.
    let Some(Value::Int(_)) = it.next() else {
        return out;
    };
    while let Some(Value::Int(key)) = it.next() {
        let Some(Value::Object(tree)) = it.next() else {
            break;
        };
        let records = tree
            .values
            .iter()
            .filter_map(Value::as_object)
            .filter_map(Annotation::from_object)
            .collect();
        out.push((*key, records));
    }
    out
}

/// Rebuild the cache from slots, restoring both counts.
fn write_slots(cache: &mut Object, slots: &[(i32, Vec<Annotation>)]) {
    let live: Vec<&(i32, Vec<Annotation>)> = slots.iter().filter(|(_, l)| !l.is_empty()).collect();
    cache.values.clear();
    if live.is_empty() {
        return; // the empty cache carries no size int
    }
    cache.values.push(Value::Int(live.len() as i32));
    for (key, list) in live {
        cache.values.push(Value::Int(*key));
        let mut tree = Object::new(INTERVAL_TREE);
        tree.values.push(Value::Int(list.len() as i32));
        for record in list {
            tree.values.push(Value::Object(record.to_object()));
        }
        cache.values.push(Value::Object(tree));
    }
}

// ---------------------------------------------------------------------------
// base64 — anchors only, so the fixed 9-byte/12-char shape is all that's needed
// ---------------------------------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len().div_ceil(3) * 4);
    for chunk in raw.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..chunk.len() + 1 {
            out.push(B64[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
        }
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.chars() {
        if c == '=' {
            break;
        }
        // Accept the URL-safe alphabet too: some tooling rewrites `+/` to `-_`.
        let c = match c {
            '-' => '+',
            '_' => '/',
            c => c,
        };
        let v = B64.iter().position(|&b| b as char == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the smallest real-shaped sidecar: a highlight in the cache, plus a
    /// record this crate has no opinion about.
    fn sample() -> Store {
        let mut cache = Object::new(ANNOTATION_CACHE);
        write_slots(
            &mut cache,
            &[(
                1,
                vec![Annotation::highlight(
                    Anchor::new(897, 0, 911),
                    Anchor::new(897, 55, 966),
                    1_786_184_401_242,
                    None,
                )],
            )],
        );
        Store {
            version: 1,
            roots: vec![
                Object {
                    name: "font.prefs".into(),
                    values: vec![
                        Value::Utf8(Some("_INVALID_,und:bookerly".into())),
                        Value::Int(-1),
                        Value::Utf8(None),
                        Value::Bool(0),
                        Value::Double([0x3f, 0x6e, 0x74, 0x1a, 0xa5, 0x97, 0x50, 0xe0]),
                        Value::Short(7),
                        Value::Float([0x40, 0x49, 0x0f, 0xdb]),
                        Value::Byte(2),
                        Value::Char(9),
                        Value::Long(-1),
                    ],
                },
                cache,
            ],
        }
    }

    #[test]
    fn every_type_round_trips_byte_for_byte() {
        let store = sample();
        let bytes = store.encode();
        let back = Store::parse(&bytes).expect("parses");
        assert_eq!(back, store, "structure survives");
        assert_eq!(back.encode(), bytes, "bytes survive");
    }

    #[test]
    fn anchors_match_the_devices_own_encoding() {
        // Lifted from real device sidecars: the base64 is what the Kindle wrote.
        for (s, eid, off, pos) in [
            ("AYEDAAAAAAAA:911", 897, 0, 911),
            ("AYEDAAA3AAAA:966", 897, 55, 966),
            ("AeYEAAAsAAAA:12937", 1254, 44, 12937),
            ("AdQFAAAAAAAA:22364", 1492, 0, 22364),
            ("AW8DAAAAAAAA:4", 879, 0, 4),
        ] {
            let a = Anchor::decode(s).expect("decodes");
            assert_eq!((a.eid, a.offset, a.position), (eid, off, pos), "{s}");
            assert_eq!(a.encode(), s, "re-encodes to the device's own bytes");
        }
    }

    #[test]
    fn a_non_anchor_string_is_not_mistaken_for_one() {
        assert!(Anchor::decode("0\u{FFFC}0").is_none());
        assert!(
            Anchor::decode("cC9KkbR1zStWRzxfccUugsw0").is_none(),
            "no colon"
        );
        assert!(Anchor::decode("AYEDAAAAAAAA:notanumber").is_none());
    }

    #[test]
    fn reads_annotations_out_of_the_cache() {
        let anns = sample().annotations();
        assert_eq!(anns.len(), 1);
        assert_eq!(anns[0].kind, Kind::Highlight);
        assert_eq!(anns[0].start().unwrap().eid, 897);
        assert_eq!(anns[0].end().unwrap().offset, 55);
        assert_eq!(anns[0].created_ms, Some(1_786_184_401_242));
        assert_eq!(anns[0].body, None, "the placeholder is not a body");
    }

    #[test]
    fn merging_adds_new_spans_keeps_old_ones_and_leaves_the_rest_alone() {
        let mut store = sample();
        let before_font = store.root("font.prefs").cloned();

        let added = store.merge_annotations(&[
            // Already there, by span — must not duplicate even with new stamps.
            Annotation::highlight(
                Anchor::new(897, 0, 911),
                Anchor::new(897, 55, 966),
                999,
                None,
            ),
            Annotation::highlight(
                Anchor::new(902, 0, 1586),
                Anchor::new(902, 104, 1690),
                5,
                Some("blue"),
            ),
        ]);
        assert_eq!(added, 1, "only the unseen span is new");

        let anns = store.annotations();
        assert_eq!(anns.len(), 2);
        assert_eq!(
            anns.iter()
                .map(|a| a.start().unwrap().position)
                .collect::<Vec<_>>(),
            vec![911, 1586],
            "ordered by start position",
        );
        assert_eq!(
            anns[0].created_ms,
            Some(1_786_184_401_242),
            "device stamp kept"
        );
        assert_eq!(
            store.root("font.prefs").cloned(),
            before_font,
            "records this crate doesn't understand are untouched",
        );
        // The rebuilt cache still declares its counts correctly.
        let cache = store.root(ANNOTATION_CACHE).unwrap();
        assert_eq!(cache.values.first(), Some(&Value::Int(1)), "one kind");
        let tree = cache.child(INTERVAL_TREE).unwrap();
        assert_eq!(tree.values.first(), Some(&Value::Int(2)), "two records");
        assert_eq!(Store::parse(&store.encode()).unwrap(), store);
    }

    #[test]
    fn merging_into_a_sidecar_with_no_annotations_builds_the_cache() {
        // The shape a device writes for a book that was read but never marked:
        // an empty cache object with no size int at all.
        let mut store = Store {
            version: 1,
            roots: vec![Object::new(ANNOTATION_CACHE)],
        };
        assert!(store.annotations().is_empty());
        let added = store.merge_annotations(&[Annotation::highlight(
            Anchor::new(10, 0, 100),
            Anchor::new(10, 5, 105),
            7,
            Some("pink"),
        )]);
        assert_eq!(added, 1);
        assert_eq!(store.annotations().len(), 1);
        assert_eq!(Store::parse(&store.encode()).unwrap(), store);
    }

    #[test]
    fn a_kind_with_no_known_slot_is_refused_rather_than_guessed() {
        let mut store = sample();
        let added = store.merge_annotations(&[Annotation {
            kind: Kind::Other("clip_article".into()),
            anchors: vec![Anchor::new(1, 0, 1), Anchor::new(1, 2, 3)],
            body: None,
            color: None,
            created_ms: Some(1),
            modified_ms: Some(1),
        }]);
        assert_eq!(added, 0);
        assert_eq!(store.annotations().len(), 1, "cache untouched");
    }

    #[test]
    fn kinds_map_to_the_slots_devices_use() {
        assert_eq!(Kind::Bookmark.cache_key(), Some(0));
        assert_eq!(Kind::Highlight.cache_key(), Some(1));
        assert_eq!(Kind::Note.cache_key(), Some(2));
        assert_eq!(Kind::Handwritten.cache_key(), Some(10));
        assert_eq!(Kind::Other("x".into()).cache_key(), None);
        assert_eq!(
            Kind::Highlight.record_name(),
            "annotation.personal.highlight"
        );
        assert_eq!(Kind::parse("handwritten_note"), Kind::Handwritten);
    }

    #[test]
    fn a_position_record_yields_its_anchor() {
        let store = Store {
            version: 1,
            roots: vec![
                Object {
                    name: "lpr".into(),
                    // Devices prefix the newer form with a version byte.
                    values: vec![
                        Value::Byte(2),
                        Value::Utf8(Some(Anchor::new(895, 0, 392).encode())),
                        Value::Long(1_786_184_407_148),
                    ],
                },
                Object {
                    name: "whisperstore.migration.status".into(),
                    values: vec![Value::Bool(0)],
                },
            ],
        };
        assert_eq!(store.position("lpr").unwrap().eid, 895);
        assert_eq!(store.position("lpr").unwrap().position, 392);
        assert!(store.position("fpr").is_none());
        assert!(store.position("whisperstore.migration.status").is_none());
    }

    #[test]
    fn a_bad_signature_is_refused() {
        assert_eq!(
            Store::parse(b"not a sidecar at all"),
            Err(KrdsError::NotASidecar)
        );
        assert!(matches!(
            Store::parse(&SIGNATURE),
            Err(KrdsError::Truncated { .. })
        ));
    }

    #[test]
    fn base64_matches_the_general_alphabet() {
        for raw in [
            vec![1u8, 129, 3, 0, 0, 55, 0, 0, 0],
            vec![0xff; 9],
            vec![0u8; 9],
        ] {
            assert_eq!(base64_decode(&base64_encode(&raw)).unwrap(), raw);
        }
        // Unpadded, and the URL-safe alphabet decodes to the same bytes.
        assert_eq!(base64_encode(&[251, 255, 190]), "+/++");
        assert_eq!(base64_decode("-_++").unwrap(), vec![251, 255, 190]);
    }
}
