//! Minimal PDF probe for the PDF→KFX path.

use std::collections::{HashMap, HashSet};
use std::io;

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::formats::pdf::doc::{decode_pdf_string, deref, load_pdf, page_geometry};

// The probed-document vocabulary lives in the format layer so both directions
// and the format-internal repairs can name it without reaching up into `import`.
pub use crate::formats::pdf::structure::{PdfDoc, PdfOutlineItem, PdfPage};

/// Probe a PDF's structure without altering its bytes.
pub fn probe_pdf(bytes: Vec<u8>) -> io::Result<PdfDoc> {
    // `load_pdf` also recovers objects lopdf silently drops from NUL-separated
    // object streams — when that bites, the catalog and entire page tree are
    // among the casualties, so it must happen before `get_pages()`.
    let doc = load_pdf(&bytes)?;

    // `get_pages()` is a BTreeMap<page_number, ObjectId>, so iterating values
    // yields pages in reading order (1..=N).
    let page_ids: Vec<ObjectId> = doc.get_pages().values().copied().collect();
    let pages: Vec<PdfPage> = page_ids
        .iter()
        .map(|&page_id| {
            let (width, height, rotation) = page_geometry(&doc, page_id);
            PdfPage {
                width,
                height,
                rotation,
            }
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

    // Map each page object to its 0-based index, then resolve the outline's
    // bookmark destinations against it.
    let page_index_of: HashMap<ObjectId, usize> = page_ids
        .iter()
        .enumerate()
        .map(|(i, &id)| (id, i))
        .collect();
    let outline = extract_outline(&doc, &page_index_of);
    let page_labels = extract_page_labels(&doc, pages.len());

    Ok(PdfDoc {
        bytes,
        pages,
        title,
        author,
        outline,
        page_labels,
    })
}

/// Read a string field from the document `/Info` dictionary, decoded to UTF-8.
fn info_string(doc: &Document, key: &[u8]) -> Option<String> {
    let info = doc.trailer.get(b"Info").ok()?;
    let dict = deref(doc, info)?.as_dict().ok()?;
    let raw = dict
        .get(key)
        .ok()
        .and_then(|o| deref(doc, o)?.as_str().ok())?;
    let s = decode_pdf_string(raw);
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Walk the document outline (`/Outlines`) into a resolved bookmark tree. Each
/// item carries its title and the 0-based page its destination jumps to. Empty
/// if the PDF has no outline or none of it resolves to a page.
fn extract_outline(
    doc: &Document,
    page_index_of: &HashMap<ObjectId, usize>,
) -> Vec<PdfOutlineItem> {
    let Ok(catalog) = doc.catalog() else {
        return Vec::new();
    };

    // Build name -> page-index for named destinations: the `/Names /Dests` name
    // tree, plus the legacy catalog `/Dests` dict.
    let mut named: HashMap<Vec<u8>, usize> = HashMap::new();
    if let Some(tree) = catalog
        .get(b"Names")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok())
        .and_then(|names| names.get(b"Dests").ok())
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok())
    {
        collect_name_tree(doc, tree, page_index_of, &mut named, 0);
    }
    if let Some(dests) = catalog
        .get(b"Dests")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok())
    {
        for (key, val) in dests.iter() {
            if let Some(pi) = resolve_dest_value(doc, val, page_index_of, 0) {
                named.entry(key.to_vec()).or_insert(pi);
            }
        }
    }

    let Some(first) = catalog
        .get(b"Outlines")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok())
        .and_then(|ol| ol.get(b"First").ok())
        .and_then(|o| o.as_reference().ok())
    else {
        return Vec::new();
    };

    let mut seen: HashSet<ObjectId> = HashSet::new();
    walk_outline_siblings(doc, first, page_index_of, &named, &mut seen, 0)
}

/// Recurse a destination name tree (`/Kids` sub-nodes + `/Names` key/value
/// pairs), recording each name's destination page.
fn collect_name_tree(
    doc: &Document,
    tree: &Dictionary,
    page_index_of: &HashMap<ObjectId, usize>,
    out: &mut HashMap<Vec<u8>, usize>,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    if let Some(kids) = tree
        .get(b"Kids")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_array().ok())
    {
        for kid in kids {
            if let Some(kd) = kid
                .as_reference()
                .ok()
                .and_then(|id| doc.get_dictionary(id).ok())
            {
                collect_name_tree(doc, kd, page_index_of, out, depth + 1);
            }
        }
    }
    if let Some(names) = tree
        .get(b"Names")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_array().ok())
    {
        // Flat [key1, val1, key2, val2, …] array.
        let mut i = 0;
        while i + 1 < names.len() {
            if let Ok(key) = names[i].as_str()
                && let Some(pi) = resolve_dest_value(doc, &names[i + 1], page_index_of, 0)
            {
                out.insert(key.to_vec(), pi);
            }
            i += 2;
        }
    }
}

/// Resolve an explicit destination *value* (a `[pageRef …]` array, or a dict
/// wrapping one under `/D`) to a page index. Used for name-tree entries, whose
/// values are never themselves named.
fn resolve_dest_value(
    doc: &Document,
    obj: &Object,
    page_index_of: &HashMap<ObjectId, usize>,
    depth: usize,
) -> Option<usize> {
    if depth > 8 {
        return None;
    }
    match deref(doc, obj)? {
        Object::Array(arr) => arr
            .first()
            .and_then(|o| o.as_reference().ok())
            .and_then(|id| page_index_of.get(&id).copied()),
        Object::Dictionary(d) => {
            resolve_dest_value(doc, d.get(b"D").ok()?, page_index_of, depth + 1)
        }
        _ => None,
    }
}

/// Resolve a bookmark destination — explicit array, named (string/name), or a
/// `/D`-wrapping dict — to a page index.
fn resolve_named_or_explicit(
    doc: &Document,
    obj: &Object,
    page_index_of: &HashMap<ObjectId, usize>,
    named: &HashMap<Vec<u8>, usize>,
    depth: usize,
) -> Option<usize> {
    if depth > 8 {
        return None;
    }
    match deref(doc, obj)? {
        Object::Array(arr) => arr
            .first()
            .and_then(|o| o.as_reference().ok())
            .and_then(|id| page_index_of.get(&id).copied()),
        Object::Name(n) => named.get(n.as_slice()).copied(),
        Object::String(s, _) => named.get(s.as_slice()).copied(),
        Object::Dictionary(d) => {
            resolve_named_or_explicit(doc, d.get(b"D").ok()?, page_index_of, named, depth + 1)
        }
        _ => None,
    }
}

/// A bookmark's destination is either a `/Dest` (explicit or named) or a `/GoTo`
/// `/A` action with `/D`. Try both.
fn resolve_outline_dest(
    doc: &Document,
    item: &Dictionary,
    page_index_of: &HashMap<ObjectId, usize>,
    named: &HashMap<Vec<u8>, usize>,
) -> Option<usize> {
    if let Ok(dest) = item.get(b"Dest")
        && let Some(pi) = resolve_named_or_explicit(doc, dest, page_index_of, named, 0)
    {
        return Some(pi);
    }
    if let Some(action) = item
        .get(b"A")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok())
        && let Ok(d) = action.get(b"D")
        && let Some(pi) = resolve_named_or_explicit(doc, d, page_index_of, named, 0)
    {
        return Some(pi);
    }
    None
}

/// Walk a sibling chain of outline items (`/Next`), recursing into `/First`
/// children. Cycle- and depth-guarded against malformed trees.
fn walk_outline_siblings(
    doc: &Document,
    first: ObjectId,
    page_index_of: &HashMap<ObjectId, usize>,
    named: &HashMap<Vec<u8>, usize>,
    seen: &mut HashSet<ObjectId>,
    depth: usize,
) -> Vec<PdfOutlineItem> {
    let mut out = Vec::new();
    if depth > 32 {
        return out;
    }
    let mut node = first;
    loop {
        if !seen.insert(node) {
            break; // cycle guard
        }
        let Ok(dict) = doc.get_dictionary(node) else {
            break;
        };

        let title = dict
            .get(b"Title")
            .ok()
            .and_then(|o| deref(doc, o))
            .and_then(|o| o.as_str().ok())
            .map(decode_pdf_string)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let own_dest = resolve_outline_dest(doc, dict, page_index_of, named);

        let children = dict
            .get(b"First")
            .ok()
            .and_then(|o| o.as_reference().ok())
            .map(|f| walk_outline_siblings(doc, f, page_index_of, named, seen, depth + 1))
            .unwrap_or_default();

        match title {
            Some(title) => {
                // Effective page: the item's own destination, else fall back to
                // its first child's page so a destination-less section header is
                // still navigable.
                let page = own_dest.or_else(|| children.first().map(|c| c.page_index));
                if let Some(page_index) = page {
                    out.push(PdfOutlineItem {
                        title,
                        page_index,
                        children,
                    });
                } else {
                    out.extend(children); // unresolvable: keep the children
                }
            }
            None => out.extend(children),
        }

        match dict.get(b"Next").ok().and_then(|o| o.as_reference().ok()) {
            Some(next) => node = next,
            None => break,
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Page labels (`/PageLabels`, PDF spec §12.4.2) → KFX `page_list` navigation.
// ---------------------------------------------------------------------------

/// How a run of pages is numbered, from one `/PageLabels` number-tree entry.
struct LabelRun {
    /// `/P` prefix (e.g. "Cover", "A-"), already decoded. May be empty.
    prefix: String,
    /// `/S` style byte: `D` decimal, `r`/`R` roman, `a`/`A` letters. `None` =
    /// no numbering (prefix only).
    style: Option<u8>,
    /// `/St` first value of this run (default 1).
    start: i64,
}

impl LabelRun {
    /// The label for the page `offset` positions into this run.
    fn label(&self, offset: usize) -> String {
        let mut s = self.prefix.clone();
        if let Some(style) = self.style {
            s.push_str(&format_page_number(self.start + offset as i64, style));
        }
        s
    }
}

/// Per-page labels for the whole document. Honors `/PageLabels`; when absent (or
/// it covers no page), falls back to sequential `"1".."N"` so the KFX always
/// gets a usable `page_list`. Always returns exactly `page_count` entries.
fn extract_page_labels(doc: &Document, page_count: usize) -> Vec<String> {
    let runs = page_label_runs(doc); // sorted by start index, ascending
    if runs.is_empty() {
        return (1..=page_count).map(|n| n.to_string()).collect();
    }
    (0..page_count)
        .map(|i| {
            // The active run is the last one whose start index is ≤ i.
            match runs.iter().rev().find(|(start, _)| *start <= i) {
                Some((start, run)) => run.label(i - start),
                // Page before the first declared run — number it plainly.
                None => (i + 1).to_string(),
            }
        })
        .collect()
}

/// Parse the catalog `/PageLabels` number tree into `(start_index, run)` pairs,
/// sorted ascending. Empty when there is no `/PageLabels`.
fn page_label_runs(doc: &Document) -> Vec<(usize, LabelRun)> {
    let Ok(catalog) = doc.catalog() else {
        return Vec::new();
    };
    let Some(root) = catalog
        .get(b"PageLabels")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_number_tree(doc, root, &mut out, 0);
    out.sort_by_key(|(start, _)| *start);
    out
}

/// Walk a number tree (`/Nums` flat `[idx, dict, …]` pairs + `/Kids` sub-nodes),
/// collecting each range's `LabelRun`. Cycle/depth guarded like the dest tree.
fn collect_number_tree(
    doc: &Document,
    node: &Dictionary,
    out: &mut Vec<(usize, LabelRun)>,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    if let Some(nums) = node
        .get(b"Nums")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_array().ok())
    {
        let mut i = 0;
        while i + 1 < nums.len() {
            if let Ok(idx) = nums[i].as_i64()
                && idx >= 0
                && let Some(dict) = deref(doc, &nums[i + 1]).and_then(|o| o.as_dict().ok())
            {
                out.push((idx as usize, parse_label_run(dict)));
            }
            i += 2;
        }
    }
    if let Some(kids) = node
        .get(b"Kids")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_array().ok())
    {
        for kid in kids {
            if let Some(kd) = kid
                .as_reference()
                .ok()
                .and_then(|id| doc.get_dictionary(id).ok())
            {
                collect_number_tree(doc, kd, out, depth + 1);
            }
        }
    }
}

/// A single `/PageLabels` label dictionary → `LabelRun`.
fn parse_label_run(dict: &Dictionary) -> LabelRun {
    let prefix = dict
        .get(b"P")
        .ok()
        .and_then(|o| o.as_str().ok())
        .map(decode_pdf_string)
        .unwrap_or_default();
    let style = dict
        .get(b"S")
        .ok()
        .and_then(|o| o.as_name().ok())
        .and_then(|n| n.first().copied());
    let start = dict
        .get(b"St")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(1);
    LabelRun {
        prefix,
        style,
        start,
    }
}

/// Format a page number per a `/PageLabels` `/S` style byte. Non-positive values
/// (or an unknown style) fall back to decimal — roman/letters aren't defined
/// there.
fn format_page_number(n: i64, style: u8) -> String {
    match style {
        b'D' => n.to_string(),
        b'R' if n > 0 => to_roman(n),
        b'r' if n > 0 => to_roman(n).to_lowercase(),
        b'A' if n > 0 => to_letters(n),
        b'a' if n > 0 => to_letters(n).to_lowercase(),
        _ => n.to_string(),
    }
}

/// Uppercase roman numeral (subtractive). `n` is assumed positive.
fn to_roman(mut n: i64) -> String {
    const TABLE: [(i64, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut s = String::new();
    for (v, sym) in TABLE {
        while n >= v {
            s.push_str(sym);
            n -= v;
        }
    }
    s
}

/// Uppercase letter label: 1→A … 26→Z, 27→AA, 28→BB, … (PDF §12.4.2). `n`
/// is assumed positive.
fn to_letters(n: i64) -> String {
    let count = ((n - 1) / 26) + 1;
    let ch = (b'A' + ((n - 1) % 26) as u8) as char;
    ch.to_string().repeat(count as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roman_numerals() {
        assert_eq!(to_roman(1), "I");
        assert_eq!(to_roman(4), "IV");
        assert_eq!(to_roman(9), "IX");
        assert_eq!(to_roman(14), "XIV");
        assert_eq!(to_roman(40), "XL");
        assert_eq!(to_roman(1989), "MCMLXXXIX");
    }

    #[test]
    fn letter_labels() {
        assert_eq!(to_letters(1), "A");
        assert_eq!(to_letters(26), "Z");
        assert_eq!(to_letters(27), "AA");
        assert_eq!(to_letters(28), "BB");
        assert_eq!(to_letters(53), "AAA");
    }

    #[test]
    fn page_number_styles_and_runs() {
        assert_eq!(format_page_number(3, b'D'), "3");
        assert_eq!(format_page_number(3, b'r'), "iii");
        assert_eq!(format_page_number(3, b'R'), "III");
        assert_eq!(format_page_number(2, b'a'), "b");
        // Unknown style / non-positive → decimal fallback.
        assert_eq!(format_page_number(5, b'?'), "5");
        assert_eq!(format_page_number(0, b'r'), "0");

        // A run mirroring Amazon's PDOC: prefix-only "Cover", then lowercase
        // roman starting at 1.
        let cover = LabelRun {
            prefix: "Cover".into(),
            style: None,
            start: 1,
        };
        assert_eq!(cover.label(0), "Cover");
        let roman = LabelRun {
            prefix: String::new(),
            style: Some(b'r'),
            start: 1,
        };
        assert_eq!(roman.label(0), "i");
        assert_eq!(roman.label(6), "vii");
        // Prefix + number (e.g. "A-1").
        let prefixed = LabelRun {
            prefix: "A-".into(),
            style: Some(b'D'),
            start: 1,
        };
        assert_eq!(prefixed.label(0), "A-1");
    }
}
