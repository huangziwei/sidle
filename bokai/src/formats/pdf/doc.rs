//! Shared PDF document core: loading, object dereferencing, and the PDF text
//! string codec.

use std::collections::{BTreeMap, HashSet};
use std::io;

use lopdf::xref::XrefEntry;
use lopdf::{Dictionary, Document, Object, ObjectId, ObjectStream, Stream, StringFormat};

/// Load a PDF from memory, repairing the object streams lopdf drops.
pub fn load_pdf(bytes: &[u8]) -> io::Result<Document> {
    let mut doc = Document::load_mem(bytes).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidData, format!("PDF parse failed: {e}"))
    })?;
    recover_nul_object_streams(&mut doc);
    Ok(doc)
}

/// Work around a lopdf limitation: an object stream (`/ObjStm`) whose index uses
fn recover_nul_object_streams(doc: &mut Document) {
    // container object id -> the compressed obj numbers lopdf failed to load.
    let mut missing: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (&id, entry) in doc.reference_table.entries.iter() {
        if let XrefEntry::Compressed { container, .. } = *entry
            && !doc.objects.contains_key(&(id, 0))
        {
            missing.entry(container).or_default().push(id);
        }
    }
    if missing.is_empty() {
        return;
    }

    let mut recovered: Vec<(ObjectId, Object)> = Vec::new();
    for (container_id, wanted) in &missing {
        let Some(Object::Stream(s)) = doc.objects.get(&(*container_id, 0)) else {
            continue;
        };
        // lopdf inflates the ObjStm in place during its (failed) load — content
        // becomes the decompressed bytes and `/Filter` is stripped — so prefer the
        // raw `content` and only inflate if it's still compressed.
        let content = s
            .decompressed_content()
            .unwrap_or_else(|_| s.content.clone());
        let Ok(first) = s.dict.get(b"First").and_then(Object::as_i64) else {
            continue;
        };
        let first = first as usize;
        if first > content.len() {
            continue;
        }
        let n = s.dict.get(b"N").and_then(Object::as_i64).unwrap_or(0);

        // NUL → space across the index so split_whitespace sees the integers.
        // Same length, so every offset in the index still lands correctly.
        let mut fixed = content;
        for b in &mut fixed[..first] {
            if *b == 0 {
                *b = b' ';
            }
        }

        // A filterless stream carrying the already-inflated, normalized bytes:
        // `ObjectStream::new` ignores the (now absent) filter and parses directly.
        let mut dict = Dictionary::new();
        dict.set("N", n);
        dict.set("First", first as i64);
        let mut stream = Stream::new(dict, fixed);
        let Ok(os) = ObjectStream::new(&mut stream) else {
            continue;
        };

        // Only adopt objects the xref says belong to THIS container (guards
        // against a stale duplicate in some other stream).
        let want: HashSet<u32> = wanted.iter().copied().collect();
        for (id, obj) in os.objects {
            if want.contains(&id.0) {
                recovered.push((id, obj));
            }
        }
    }

    for (id, obj) in recovered {
        doc.objects.entry(id).or_insert(obj);
    }
}

/// Resolve an object one indirection deep (a value may be an inline object or
/// a `Reference` into the object table).
pub(crate) fn deref<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Object> {
    match obj {
        Object::Reference(id) => doc.get_object(*id).ok(),
        other => Some(other),
    }
}

/// Decode a PDF text string (used for `/Info` and outline titles). Two encodings
/// occur: UTF-16BE (marked by a `FE FF` BOM) or PDFDocEncoding.
pub(crate) fn decode_pdf_string(b: &[u8]) -> String {
    if b.len() >= 2 && b[0] == 0xFE && b[1] == 0xFF {
        let units: Vec<u16> = b[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        b.iter().map(|&c| pdfdoc_to_char(c)).collect()
    }
}

/// Encode a Rust string as a PDF text string object — the inverse of
/// [`decode_pdf_string`], for the write side.
pub(crate) fn encode_pdf_string(s: &str) -> Object {
    if s.is_ascii() {
        return Object::String(s.as_bytes().to_vec(), StringFormat::Literal);
    }
    let mut bytes = vec![0xFE, 0xFF];
    for unit in s.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    Object::String(bytes, StringFormat::Hexadecimal)
}

/// Map one PDFDocEncoding byte to its Unicode char. PDFDocEncoding (PDF 32000-1
fn pdfdoc_to_char(b: u8) -> char {
    match b {
        0x18 => '\u{02D8}', // breve
        0x19 => '\u{02C7}', // caron
        0x1A => '\u{02C6}', // circumflex
        0x1B => '\u{02D9}', // dotaccent
        0x1C => '\u{02DD}', // hungarumlaut
        0x1D => '\u{02DB}', // ogonek
        0x1E => '\u{02DA}', // ring
        0x1F => '\u{02DC}', // tilde
        0x80 => '\u{2022}', // bullet
        0x81 => '\u{2020}', // dagger
        0x82 => '\u{2021}', // daggerdbl
        0x83 => '\u{2026}', // ellipsis
        0x84 => '\u{2014}', // emdash
        0x85 => '\u{2013}', // endash
        0x86 => '\u{0192}', // florin
        0x87 => '\u{2044}', // fraction
        0x88 => '\u{2039}', // guilsinglleft
        0x89 => '\u{203A}', // guilsinglright
        0x8A => '\u{2212}', // minus
        0x8B => '\u{2030}', // perthousand
        0x8C => '\u{201E}', // quotedblbase
        0x8D => '\u{201C}', // quotedblleft
        0x8E => '\u{201D}', // quotedblright
        0x8F => '\u{2018}', // quoteleft
        0x90 => '\u{2019}', // quoteright
        0x91 => '\u{201A}', // quotesinglbase
        0x92 => '\u{2122}', // trademark
        0x93 => '\u{FB01}', // fi ligature
        0x94 => '\u{FB02}', // fl ligature
        0x95 => '\u{0141}', // Lslash
        0x96 => '\u{0152}', // OE
        0x97 => '\u{0160}', // Scaron
        0x98 => '\u{0178}', // Ydieresis
        0x99 => '\u{017D}', // Zcaron
        0x9A => '\u{0131}', // dotlessi
        0x9B => '\u{0142}', // lslash
        0x9C => '\u{0153}', // oe
        0x9D => '\u{0161}', // scaron
        0x9E => '\u{017E}', // zcaron
        0x9F => '\u{FFFD}', // undefined in PDFDocEncoding
        0xA0 => '\u{20AC}', // euro
        // 0x00–0x17, 0x20–0x7E (ASCII) and 0xA1–0xFF agree with Latin-1.
        other => other as char,
    }
}

/// Default page size when a PDF declares no MediaBox anywhere in the tree:
/// US Letter (612×792 pt), matching lopdf's own creator default.
const DEFAULT_MEDIABOX: [f32; 4] = [0.0, 0.0, 612.0, 792.0];

/// Compute a page's displayed geometry: resolve `/MediaBox` (walking the
/// `/Pages` tree for the inherited value), apply `/Rotate` to the extents, and
/// report the rotation as quarter turns clockwise.
pub(crate) fn page_geometry(doc: &Document, page_id: ObjectId) -> (f32, f32, u8) {
    let mut media: Option<[f32; 4]> = None;
    let mut rotate: i64 = 0;
    let mut found_rotate = false;

    // Walk the page node up through its `/Parent` chain. Both MediaBox and
    // Rotate are inheritable page-tree attributes.
    let mut node: Option<ObjectId> = Some(page_id);
    let mut seen: HashSet<ObjectId> = HashSet::new();
    while let Some(id) = node {
        if !seen.insert(id) {
            break; // cycle guard
        }
        let Ok(dict) = doc.get_dictionary(id) else {
            break;
        };

        if media.is_none()
            && let Ok(mb) = dict.get(b"MediaBox")
            && let Some(arr) = deref(doc, mb).and_then(|o| o.as_array().ok())
            && arr.len() == 4
        {
            let v: Vec<f32> = arr
                .iter()
                .filter_map(|o| deref(doc, o).and_then(|x| x.as_float().ok()))
                .collect();
            if v.len() == 4 {
                media = Some([v[0], v[1], v[2], v[3]]);
            }
        }

        if !found_rotate
            && let Ok(r) = dict.get(b"Rotate")
            && let Some(n) = deref(doc, r).and_then(|o| o.as_i64().ok())
        {
            rotate = n;
            found_rotate = true;
        }

        node = dict.get(b"Parent").and_then(|o| o.as_reference()).ok();
    }

    let [llx, lly, urx, ury] = media.unwrap_or(DEFAULT_MEDIABOX);
    let mut w = (urx - llx).abs();
    let mut h = (ury - lly).abs();

    // Normalize rotation to whole quarter turns in 0..=3 and swap the axes for
    // the odd ones. A `/Rotate` that isn't a multiple of 90 is invalid; round it
    // to the nearest quarter turn rather than dropping the page.
    let quarters = ((rotate as f64 / 90.0).round() as i64).rem_euclid(4) as u8;
    if quarters % 2 == 1 {
        std::mem::swap(&mut w, &mut h);
    }

    // Guard against degenerate boxes.
    if w <= 0.0 || h <= 0.0 {
        (612.0, 792.0, quarters)
    } else {
        (w, h, quarters)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_pdfdoc_typographic_bytes() {
        // 0x8F/0x90 are curly single quotes in PDFDocEncoding, not C1 control
        // codes. 0x97=Scaron, 0x96=OE.
        let s = decode_pdf_string(b"\x8fHello\x90 \x97 \x96 \x84dash");
        assert_eq!(s, "\u{2018}Hello\u{2019} \u{0160} \u{0152} \u{2014}dash");
    }

    #[test]
    fn decode_utf16be_with_bom() {
        let s = decode_pdf_string(&[0xFE, 0xFF, 0x00, b'H', 0x00, b'i']);
        assert_eq!(s, "Hi");
    }

    #[test]
    fn decode_ascii_and_latin1_unchanged() {
        assert_eq!(decode_pdf_string(b"Plain ASCII"), "Plain ASCII");
        // 0xE9 = é in both Latin-1 and PDFDocEncoding.
        assert_eq!(decode_pdf_string(b"caf\xe9"), "caf\u{00E9}");
    }

    /// ASCII encodes to a plain literal; non-ASCII to UTF-16BE hex with a BOM.
    #[test]
    fn encode_picks_literal_for_ascii_and_utf16_otherwise() {
        match encode_pdf_string("Plain Title") {
            Object::String(b, StringFormat::Literal) => assert_eq!(b, b"Plain Title"),
            other => panic!("expected a literal string, got {other:?}"),
        }
        match encode_pdf_string("人間失格") {
            Object::String(b, StringFormat::Hexadecimal) => {
                assert_eq!(&b[..2], &[0xFE, 0xFF], "UTF-16BE BOM");
            }
            other => panic!("expected a hex string, got {other:?}"),
        }
    }

    /// The codec round-trips: whatever we encode, `decode_pdf_string` recovers.
    /// This is the property the metadata editor depends on.
    #[test]
    fn encode_decode_roundtrip() {
        for s in [
            "Plain ASCII Title",
            "人間失格",
            "Caf\u{e9} — “Quoted”",
            "Q&A: <Draft> (v2)",
            "",
            "\u{1F600} emoji beyond the BMP",
        ] {
            let Object::String(bytes, _) = encode_pdf_string(s) else {
                panic!("encode must produce a string object");
            };
            assert_eq!(decode_pdf_string(&bytes), s, "round-trip for {s:?}");
        }
    }

    #[test]
    fn recovers_objects_from_nul_separated_object_stream() {
        use lopdf::xref::XrefEntry;
        use lopdf::{Document, Stream};

        // A decompressed ObjStm payload whose index uses NUL (0x00) as the
        // separator — the exact shape lopdf's split_whitespace silently drops.
        // Two objects: 10 -> Boolean(true) at objects-offset 0, 11 -> Integer(42)
        let mut content: Vec<u8> = Vec::new();
        content.extend_from_slice(b"10\x000\x0011\x005"); // index pairs (10,0) (11,5)
        let first = content.len() as i64;
        content.extend_from_slice(b"true 42");

        let mut dict = Dictionary::new();
        dict.set("Type", Object::Name(b"ObjStm".to_vec()));
        dict.set("N", 2i64);
        dict.set("First", first);
        // No /Filter: stands in for the stream lopdf already inflated in place
        // (it strips the filter when it decompresses during load).

        let mut doc = Document::new();
        doc.objects
            .insert((2, 0), Object::Stream(Stream::new(dict, content)));
        doc.reference_table.entries.insert(
            10,
            XrefEntry::Compressed {
                container: 2,
                index: 0,
            },
        );
        doc.reference_table.entries.insert(
            11,
            XrefEntry::Compressed {
                container: 2,
                index: 1,
            },
        );

        // Precondition: the compressed objects are unresolved (as after a real
        // lopdf load that hit the NUL-index bug).
        assert!(!doc.objects.contains_key(&(10, 0)));

        recover_nul_object_streams(&mut doc);

        assert!(matches!(doc.get_object((10, 0)), Ok(Object::Boolean(true))));
        assert!(matches!(doc.get_object((11, 0)), Ok(Object::Integer(42))));
    }

    #[test]
    fn recover_is_noop_without_missing_compressed_objects() {
        let mut doc = Document::new();
        let before = doc.objects.len();
        recover_nul_object_streams(&mut doc); // must not panic or change anything
        assert_eq!(doc.objects.len(), before);
    }

    #[test]
    fn load_pdf_rejects_non_pdf_bytes() {
        assert!(load_pdf(b"not a pdf at all").is_err());
    }
}
