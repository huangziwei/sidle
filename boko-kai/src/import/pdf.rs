//! Minimal PDF probe for the PDF→KFX path.
//!
//! We do *not* convert PDF content. Amazon's "Send to Kindle" wraps the PDF
//! verbatim inside a KFX container and lets the device render each page (which
//! is what makes the Scribe pen draw over it). So all we need from the PDF is
//! the structural shape of a fixed-layout book:
//!
//! - page **count**,
//! - each page's **MediaBox** size in points (with `/Rotate` applied and
//!   inheritance from the page tree resolved), and
//! - the document `/Info` **title** / **author** (best effort).
//!
//! The original bytes are carried through untouched for embedding.

use std::collections::HashSet;
use std::io;

use lopdf::{Document, Object, ObjectId};

/// One PDF page's display size, in PDF points (1/72 inch).
#[derive(Debug, Clone, Copy)]
pub struct PdfPage {
    pub width: f32,
    pub height: f32,
}

/// A probed PDF: the verbatim bytes plus the structural facts the KFX writer
/// needs. `bytes` is the unmodified input — embed it as-is.
#[derive(Debug, Clone)]
pub struct PdfDoc {
    pub bytes: Vec<u8>,
    pub pages: Vec<PdfPage>,
    pub title: Option<String>,
    pub author: Option<String>,
}

/// Default page size when a PDF declares no MediaBox anywhere in the tree:
/// US Letter (612×792 pt), matching lopdf's own creator default.
const DEFAULT_MEDIABOX: [f32; 4] = [0.0, 0.0, 612.0, 792.0];

/// Probe a PDF's structure without altering its bytes.
pub fn probe_pdf(bytes: Vec<u8>) -> io::Result<PdfDoc> {
    let doc = Document::load_mem(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("PDF parse failed: {e}")))?;

    // `get_pages()` is a BTreeMap<page_number, ObjectId>, so iterating values
    // yields pages in reading order (1..=N).
    let pages: Vec<PdfPage> = doc
        .get_pages()
        .values()
        .map(|&page_id| {
            let (width, height) = page_dimensions(&doc, page_id);
            PdfPage { width, height }
        })
        .collect();

    if pages.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PDF has no pages",
        ));
    }

    let title = info_string(&doc, b"Title");
    let author = info_string(&doc, b"Author");

    Ok(PdfDoc {
        bytes,
        pages,
        title,
        author,
    })
}

/// Resolve an object one indirection deep (a value may be an inline object or
/// a `Reference` into the object table).
fn deref<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Object> {
    match obj {
        Object::Reference(id) => doc.get_object(*id).ok(),
        other => Some(other),
    }
}

/// Compute a page's displayed size in points: resolve `/MediaBox` (walking the
/// `/Pages` tree for the inherited value) and apply `/Rotate`.
fn page_dimensions(doc: &Document, page_id: ObjectId) -> (f32, f32) {
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

    // Normalize rotation to [0,360) and swap axes for quarter turns.
    let rot = rotate.rem_euclid(360);
    if rot == 90 || rot == 270 {
        std::mem::swap(&mut w, &mut h);
    }

    // Guard against degenerate boxes.
    if w <= 0.0 || h <= 0.0 {
        (612.0, 792.0)
    } else {
        (w, h)
    }
}

/// Read a string field from the document `/Info` dictionary, decoded to UTF-8.
fn info_string(doc: &Document, key: &[u8]) -> Option<String> {
    let info = doc.trailer.get(b"Info").ok()?;
    let dict = deref(doc, info)?.as_dict().ok()?;
    let raw = dict.get(key).ok().and_then(|o| deref(doc, o)?.as_str().ok())?;
    let s = decode_pdf_string(raw);
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Decode a PDF text string. Two encodings occur in `/Info`:
/// - UTF-16BE, marked by a `FE FF` byte-order mark, or
/// - PDFDocEncoding, which agrees with Latin-1 across the range real titles use.
fn decode_pdf_string(b: &[u8]) -> String {
    if b.len() >= 2 && b[0] == 0xFE && b[1] == 0xFF {
        let units: Vec<u16> = b[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        b.iter().map(|&c| c as char).collect()
    }
}
