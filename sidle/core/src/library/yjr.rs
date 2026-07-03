//! Parser for Kindle in-book annotation records
//! (`documents/<book>.sdr/<book>.yjr`).
//!
//! The `.yjr` is Amazon's custom length-prefixed key/value container (NOT
//! Ion-binary). Reverse-engineered in P0 and verified against the real device
//! (see `.claude/plans/sidle-reader.md`). The byte grammar is a flat stream of
//! tokens, each `[marker:1][len:3 big-endian][payload:len bytes]`:
//!
//!   * `0xfe` — a key/symbol (`annotation.personal.highlight`, `font.prefs`, …).
//!   * `0x03` — a UTF-8 string value.
//!   * `0x02` — an int/timestamp (skipped; not needed for annotations).
//!
//! An `annotation.personal.{highlight,note,bookmark}` key is followed by its
//! string values: a start handle, an end handle, a `U+FFFC` placeholder for the
//! (absent) highlighted text, and — for notes — the typed body inline. Each
//! handle is `base64(type:u8, eid:u32-LE, offset:u32-LE) ":" linear_position`;
//! the base64 is the authoritative anchor, the trailing int a derived linear
//! position (feeds the human "Location" / Whispersync).
//!
//! **Highlight/bookmark text is NOT in the `.yjr`** (the placeholder is
//! `U+FFFC`) — it is recovered as the book substring between the anchors via the
//! boko KFX→DOM `eid→text` map (see `anchor.rs`). Note *bodies* ARE inline here.
//!
//! Scope: annotation records (`.yjr`) plus the `.yjf` sidecar's last-read
//! **position**. The `.yjf` is the same container, and its `lpr` (last page
//! read) / `fpr` (first) keys carry the same `base64(type,eid,offset):linear`
//! handle as an annotation anchor — decoded by [`decode_position`]. (The
//! `.yjr`'s own `sync_lpr` was a dead end — a 3-byte `00 01 ff` token with no
//! anchor — so the real device position lives in the `.yjf`.)

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;

const MARKER_STRING: u8 = 0x03;
const MARKER_KEY: u8 = 0xfe;
/// Sanity bound on a token payload — real `.yjr` strings are short; anything
/// larger is a misread we resync past rather than trust.
const MAX_TOKEN_LEN: usize = 8192;
/// `U+FFFC OBJECT REPLACEMENT CHARACTER` — Kindle's stand-in for the (omitted)
/// highlighted text inside an annotation record.
const PLACEHOLDER: char = '\u{FFFC}';

/// Which kind of `annotation.personal.*` record this is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Highlight,
    Note,
    Bookmark,
    /// Handwritten ink drawn on a sideloaded doc (`annotation.personal.handwritten_note`).
    /// One record per drawn page: its anchor handle is the host-page position, and
    /// its inline "body" is the ink notebook's page-container kfx_id (the per-page
    /// link). Routed to the ink path ([`crate::library::ink`]), never the text
    /// `annotations` table — there is no covered text to extract.
    Handwritten,
    /// Any future/unknown `annotation.personal.<x>` — kept verbatim, not dropped.
    Other(String),
}

impl Kind {
    /// Map a lowercase kind label to a `Kind`. The label is the
    /// `annotation.personal.<suffix>` segment in a `.yjr`, and equivalently the
    /// lowercased `Your <Kind>` word in `My Clippings.txt` — the same three
    /// names, so `clippings.rs` reuses this rather than duplicating the match.
    pub fn parse(suffix: &str) -> Self {
        match suffix {
            "highlight" => Kind::Highlight,
            "note" => Kind::Note,
            "bookmark" => Kind::Bookmark,
            "handwritten_note" => Kind::Handwritten,
            other => Kind::Other(other.to_string()),
        }
    }

    /// Stable lowercase tag for the DB `kind` column.
    pub fn as_str(&self) -> &str {
        match self {
            Kind::Highlight => "highlight",
            Kind::Note => "note",
            Kind::Bookmark => "bookmark",
            Kind::Handwritten => "handwritten_note",
            Kind::Other(s) => s,
        }
    }
}

/// A decoded annotation anchor: `base64(type, eid, offset):linear`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    /// Leading type byte — `0x01` in every record observed.
    pub type_byte: u8,
    /// KFX element id (the `loc_id`/`$266` the reader resolves to a DOM element).
    pub eid: u32,
    /// Base-text char offset within the element (ruby-independent; see P0).
    pub offset: u32,
    /// Derived linear position (human "Location" / Whispersync); a sort key.
    pub linear: u64,
    /// The raw base64 text, kept for dedup hashing / debugging.
    pub b64: String,
}

/// One `annotation.personal.*` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub kind: Kind,
    /// `[start, end]` for highlights/notes; `[pos, pos]` for bookmarks. Empty
    /// only for a malformed record with no decodable handle.
    pub handles: Vec<Handle>,
    /// Typed note body, stored inline in the `.yjr` (notes only).
    pub note_body: Option<String>,
}

impl Annotation {
    /// Start anchor (`handles[0]`).
    pub fn start(&self) -> Option<&Handle> {
        self.handles.first()
    }

    /// End anchor (`handles[1]`); bookmarks repeat the start, so fall back to it.
    pub fn end(&self) -> Option<&Handle> {
        self.handles.get(1).or_else(|| self.handles.first())
    }
}

/// Split the container into `(marker, utf8 payload)` tokens. Mirrors the proven
/// P0 scanner: walk byte-by-byte, and at a `0x03`/`0xfe` marker try to read a
/// length-prefixed UTF-8 payload; on any failure advance one byte and resync.
/// Non-string tokens (`0x02` ints) are simply never matched.
fn tokens(data: &[u8]) -> Vec<(u8, &str)> {
    let mut out = Vec::new();
    let n = data.len();
    let mut i = 0usize;
    while i + 4 <= n {
        let marker = data[i];
        if marker == MARKER_STRING || marker == MARKER_KEY {
            let len = ((data[i + 1] as usize) << 16)
                | ((data[i + 2] as usize) << 8)
                | (data[i + 3] as usize);
            if len > 0
                && len < MAX_TOKEN_LEN
                && i + 4 + len <= n
                && let Ok(s) = std::str::from_utf8(&data[i + 4..i + 4 + len])
            {
                out.push((marker, s));
                i += 4 + len;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Decode a `base64(type, eid:u32-LE, offset:u32-LE):linear` handle. Returns
/// `None` for anything that isn't a valid 9-byte handle (e.g. a note body or the
/// placeholder), which is how `parse` distinguishes handles from inline text.
fn decode_handle(s: &str) -> Option<Handle> {
    let (b64, linear) = s.split_once(':')?;
    let linear: u64 = linear.parse().ok()?;
    // Accept both the standard (`+/`) and url-safe (`-_`) alphabets; handles are
    // 9 bytes → 12 chars, never padded.
    let norm: String = b64
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .filter(|&c| c != '=')
        .collect();
    let raw = STANDARD_NO_PAD.decode(&norm).ok()?;
    if raw.len() != 9 {
        return None;
    }
    Some(Handle {
        type_byte: raw[0],
        eid: u32::from_le_bytes([raw[1], raw[2], raw[3], raw[4]]),
        offset: u32::from_le_bytes([raw[5], raw[6], raw[7], raw[8]]),
        linear,
        b64: b64.to_string(),
    })
}

/// Parse a `.yjr` byte buffer into its annotation records. Non-annotation keys
/// (`font.prefs`, `ReaderMetrics`, `next.in.series.info.data`, …) are ignored;
/// a file with no annotations yields an empty `Vec`.
pub fn parse(bytes: &[u8]) -> Vec<Annotation> {
    let mut anns: Vec<Annotation> = Vec::new();
    // Index of the record currently accepting values, or `None` between records
    // / inside a non-annotation key.
    let mut cur: Option<usize> = None;
    for (marker, txt) in tokens(bytes) {
        match marker {
            MARKER_KEY => {
                cur = match txt.strip_prefix("annotation.personal.") {
                    Some(suffix) => {
                        anns.push(Annotation {
                            kind: Kind::parse(suffix),
                            handles: Vec::new(),
                            note_body: None,
                        });
                        Some(anns.len() - 1)
                    }
                    None => None,
                };
            }
            MARKER_STRING => {
                if let Some(idx) = cur {
                    if let Some(handle) = decode_handle(txt) {
                        anns[idx].handles.push(handle);
                    } else if !txt.contains(PLACEHOLDER) && !txt.trim().is_empty() {
                        anns[idx].note_body = Some(txt.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    anns
}

/// Read and parse a `.yjr` file.
pub fn parse_file(path: &std::path::Path) -> std::io::Result<Vec<Annotation>> {
    Ok(parse(&std::fs::read(path)?))
}

/// Decode a named position handle from a `.yjf` sidecar — `lpr` (last page read)
/// or `fpr` (first/start). The `.yjf` shares the `.yjr` container grammar; the
/// key is a top-level `0xfe` symbol whose following `0x03` value is the same
/// `base64(type,eid,offset):linear` handle as an annotation anchor. Returns the
/// first decodable handle after `key`, or `None` if the key/handle is absent
/// (so a state-only `.yjf` with no `lpr` is simply skipped).
pub fn decode_position(bytes: &[u8], key: &str) -> Option<Handle> {
    let mut armed = false;
    for (marker, txt) in tokens(bytes) {
        match marker {
            MARKER_KEY => armed = txt == key,
            MARKER_STRING if armed => {
                if let Some(handle) = decode_handle(txt) {
                    return Some(handle);
                }
                armed = false; // the value after `key` wasn't a handle; stop waiting
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*; // also re-exports the parent's `base64::Engine` + `STANDARD_NO_PAD`

    /// Build one `[marker][len:3 BE][payload]` token.
    fn tok(marker: u8, payload: &[u8]) -> Vec<u8> {
        let len = payload.len();
        let mut v = vec![marker, (len >> 16) as u8, (len >> 8) as u8, len as u8];
        v.extend_from_slice(payload);
        v
    }
    fn key(name: &str) -> Vec<u8> {
        tok(MARKER_KEY, name.as_bytes())
    }
    fn val(s: &str) -> Vec<u8> {
        tok(MARKER_STRING, s.as_bytes())
    }
    /// Encode a handle exactly as the device does: base64(01, eid LE, off LE):lin.
    fn handle(eid: u32, off: u32, linear: u64) -> String {
        let mut raw = vec![1u8];
        raw.extend_from_slice(&eid.to_le_bytes());
        raw.extend_from_slice(&off.to_le_bytes());
        format!("{}:{}", STANDARD_NO_PAD.encode(&raw), linear)
    }

    #[test]
    fn decodes_known_real_handles() {
        // Lifted from the real 文学少女 / 十角館 .yjr (the P0 oracle).
        let h = decode_handle("AeYEAAAsAAAA:12937").unwrap();
        assert_eq!(
            (h.type_byte, h.eid, h.offset, h.linear),
            (1, 1254, 44, 12937)
        );
        let h = decode_handle("AekEAABEAAAA:13054").unwrap();
        assert_eq!((h.eid, h.offset), (1257, 68));
        let h = decode_handle("AdQFAAAAAAAA:22364").unwrap();
        assert_eq!((h.eid, h.offset, h.linear), (1492, 0, 22364));
    }

    #[test]
    fn handle_encoder_matches_device() {
        // Our test encoder must reproduce the real device's base64 byte-for-byte,
        // else the synthetic fixtures below would prove nothing.
        assert_eq!(handle(1254, 44, 12937), "AeYEAAAsAAAA:12937");
        let h = decode_handle(&handle(926, 1, 184)).unwrap();
        assert_eq!((h.eid, h.offset, h.linear), (926, 1, 184));
    }

    #[test]
    fn parses_highlight_note_bookmark() {
        // A `saved.avl.interval.tree` key precedes each record on the device.
        let bytes: Vec<u8> = [
            key("font.prefs"),
            val("_INVALID_,ja:tbmincho"), // value of a non-annotation key → ignored
            key("saved.avl.interval.tree"),
            key("annotation.personal.highlight"),
            val(&handle(1254, 44, 12937)),
            val(&handle(1257, 68, 13054)),
            val("0\u{FFFC}0"), // placeholder, must NOT become a body
            key("annotation.personal.note"),
            val(&handle(100, 5, 200)),
            val(&handle(100, 9, 201)),
            val("0\u{FFFC}0"),
            val("Digit normalization required"), // inline typed body
            key("annotation.personal.bookmark"),
            val(&handle(1492, 0, 22364)),
            val(&handle(1492, 0, 22364)),
            val("0\u{FFFC}0"),
            key("ReaderMetrics"),
            val("booklaunchedbefore"),
            val("true"),
        ]
        .concat();
        let anns = parse(&bytes);
        assert_eq!(anns.len(), 3);

        let hl = &anns[0];
        assert_eq!(hl.kind, Kind::Highlight);
        assert_eq!(
            (hl.start().unwrap().eid, hl.start().unwrap().offset),
            (1254, 44)
        );
        assert_eq!(
            (hl.end().unwrap().eid, hl.end().unwrap().offset),
            (1257, 68)
        );
        assert_eq!(hl.note_body, None);

        let note = &anns[1];
        assert_eq!(note.kind, Kind::Note);
        assert_eq!(
            note.note_body.as_deref(),
            Some("Digit normalization required")
        );
        assert_eq!(
            (note.start().unwrap().eid, note.end().unwrap().offset),
            (100, 9)
        );

        let bm = &anns[2];
        assert_eq!(bm.kind, Kind::Bookmark);
        assert_eq!(bm.start().unwrap().eid, 1492);
        assert_eq!(bm.end().unwrap().eid, 1492); // repeats the start
        assert_eq!(bm.note_body, None);
    }

    #[test]
    fn parses_handwritten_note_with_container_id_body() {
        // One drawn page on a sideloaded doc: a `handwritten_note` record with a
        // host-page anchor handle followed by the nbk page-container kfx_id as the
        // inline "body" (it has no ':' so it never decodes as a handle — it falls
        // through to note_body, exactly the per-page link Sidle joins on).
        let bytes: Vec<u8> = [
            key("annotation.personal.handwritten_note"),
            val(&handle(1158, 0, 9782)),
            val("cC9KkbR1zStWRzxfccUugsw0"),
        ]
        .concat();
        let anns = parse(&bytes);
        assert_eq!(anns.len(), 1);
        let hw = &anns[0];
        assert_eq!(hw.kind, Kind::Handwritten);
        assert_eq!(hw.kind.as_str(), "handwritten_note");
        assert_eq!(
            (hw.start().unwrap().eid, hw.start().unwrap().linear),
            (1158, 9782)
        );
        // The body is the container id, NOT a decoded handle.
        assert_eq!(hw.note_body.as_deref(), Some("cC9KkbR1zStWRzxfccUugsw0"));
    }

    #[test]
    fn ignores_files_without_annotations() {
        // Shape of the 5 real "state-only" books (no highlights/notes/bookmarks).
        // `annotation.cache.object` must NOT be mistaken for an annotation.
        let bytes: Vec<u8> = [
            key("font.prefs"),
            val("_INVALID_,ja:tbmincho"),
            key("sync_lpr"),
            key("next.in.series.info.data"),
            val("{\"asin\":\"B00Q5YXMQO\"}"),
            key("annotation.cache.object"),
            key("ReaderMetrics"),
            val("booklaunchedbefore"),
            val("true"),
        ]
        .concat();
        assert!(parse(&bytes).is_empty());
    }

    #[test]
    fn resyncs_past_garbage() {
        // A stray 0x03 with an oversized length must not derail the scan.
        let mut bytes = vec![MARKER_STRING, 0xff, 0xff, 0xff];
        bytes.extend(key("annotation.personal.bookmark"));
        bytes.extend(val(&handle(7, 0, 9)));
        let anns = parse(&bytes);
        assert_eq!(anns.len(), 1);
        assert_eq!(anns[0].start().unwrap().eid, 7);
    }

    #[test]
    fn decodes_yjf_lpr_position() {
        // A `.yjf` is the same container: `lpr`/`fpr` keys each followed by a
        // handle value, interleaved with non-position keys (timer.*) that must
        // not be picked up. `lpr` values mirror ブギーポップ (eid 978, off 170 —
        // a character-precise mid-element position).
        let bytes: Vec<u8> = [
            key("fpr"),
            val(&handle(910, 0, 100)),
            key("lpr"),
            val(&handle(978, 170, 12345)),
            key("timer.average.calculator.outliers"),
            val("3"),
        ]
        .concat();

        let lpr = decode_position(&bytes, "lpr").expect("lpr present");
        assert_eq!((lpr.eid, lpr.offset, lpr.linear), (978, 170, 12345));
        let fpr = decode_position(&bytes, "fpr").expect("fpr present");
        assert_eq!((fpr.eid, fpr.offset), (910, 0));
        // An absent key (or a state-only `.yjf`) yields nothing, never a misread.
        assert!(decode_position(&bytes, "sync_lpr").is_none());
        assert!(decode_position(b"", "lpr").is_none());
    }
}
