//! Reading order that contradicts the book's own navigation.

use std::collections::HashMap;
use std::io;

use crate::formats::epub::edit::{EpubPackage, attr_value};
use crate::formats::epub::structure::{basename, dir_of};
use crate::formats::epub::toc_repair::existing_declared_toc;
use crate::formats::epub::{OpfData, parse_opf};
use crate::util::{decode_text, extract_xml_encoding, percent_decode};

/// One spine document, as a reorder panel and the writer both need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpineDoc {
    /// The manifest id the OPF's `<itemref idref>` names. The writer's only
    /// handle — an order is a list of these.
    pub idref: String,
    /// Absolute zip path, for display when the book gives no better name.
    pub href: String,
    /// The declared-TOC label that targets this document, when one does. The
    /// first such entry wins: a chapter split across scenes points several
    /// entries at one file, and the first is the chapter's own.
    pub label: Option<String>,
}

/// How far a book's spine has drifted from its own declared navigation — see
/// [`declared_spine_misordering`]. All zeros means the two agree, which is the
/// common case and says nothing needs doing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Misordering {
    /// Places where the declared TOC's next entry sits *earlier* in the spine
    /// than the entry before it. Each one is a point where the two orders
    /// cannot both be right.
    pub descents: usize,
    /// How many spine documents [`propose_spine`] would move. Zero whenever
    /// `descents` is zero, and the number a confirmation prompt should quote.
    pub moved: usize,
    /// The spine is its own manifest in lexicographic order — by `idref` or by
    /// filename. Books are not authored this way; a packaging tool sorted them.
    /// On its own this decides which side is wrong: the spine.
    pub machine_sorted: bool,
    /// The first entry the spine reads *late*: the TOC lists it before the entry
    /// after it, and the spine puts it after.
    pub first_out_of_order: Option<String>,
}

impl Misordering {
    /// Whether the book contradicts itself about its own reading order.
    pub fn contradicts(&self) -> bool {
        self.descents > 0
    }
}

/// Measure what [`propose_spine`] would move: a spine whose order disagrees
/// with the order the book's own declared TOC lists its chapters in.
pub fn declared_spine_misordering(epub_bytes: &[u8]) -> io::Result<Misordering> {
    Ok(analyze(epub_bytes)?.misordering)
}

/// The spine in the order the book's own navigation implies, ready for a human
pub fn propose_spine(epub_bytes: &[u8]) -> io::Result<Vec<SpineDoc>> {
    let a = analyze(epub_bytes)?;
    Ok(a.proposed.into_iter().map(|i| a.docs[i].clone()).collect())
}

/// The spine as the book currently declares it, in reading order.
pub fn current_spine(epub_bytes: &[u8]) -> io::Result<Vec<SpineDoc>> {
    Ok(analyze(epub_bytes)?.docs)
}

/// Write `order` — a list of manifest ids — as the book's spine.
pub fn set_spine(epub_bytes: &[u8], order: &[String]) -> io::Result<Vec<u8>> {
    let mut pkg = EpubPackage::parse(epub_bytes)?;
    let opf_path = pkg.opf_path()?;
    let opf_raw =
        decode_text(pkg.opf_bytes()?, extract_xml_encoding(pkg.opf_bytes()?)).into_owned();
    let opf = parse_opf(&opf_raw).map_err(io::Error::other)?;

    if !is_permutation(&opf.spine_ids, order) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a spine reorder must name exactly the documents the spine already \
             names — adding or removing one is a different edit",
        ));
    }
    if opf.spine_ids == order {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the spine is already in this order",
        ));
    }

    let rewritten = permute_itemrefs(&opf_raw, order)?;
    pkg.replace(&opf_path, rewritten.into_bytes());
    pkg.into_bytes()
}

/// One-call repair: [`propose_spine`] then [`set_spine`]. Errors when the book
/// evidences no reordering — the caller is expected to have asked first.
pub fn repair_spine(epub_bytes: &[u8]) -> io::Result<Vec<u8>> {
    let order: Vec<String> = propose_spine(epub_bytes)?
        .into_iter()
        .map(|d| d.idref)
        .collect();
    set_spine(epub_bytes, &order)
}

// ---------------------------------------------------------------------------
// The analysis both the measurement and the proposal read
// ---------------------------------------------------------------------------

struct Analysis {
    docs: Vec<SpineDoc>,
    /// Indices into `docs`, in the proposed reading order.
    proposed: Vec<usize>,
    misordering: Misordering,
}

fn analyze(epub_bytes: &[u8]) -> io::Result<Analysis> {
    let pkg = EpubPackage::parse(epub_bytes)?;
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let opf_str = decode_text(pkg.opf_bytes()?, extract_xml_encoding(pkg.opf_bytes()?));
    let opf = parse_opf(&opf_str).map_err(io::Error::other)?;

    // Spine documents, and the file each one is reached by. A nav href names a
    // file, so that is the vocabulary the two orders are compared in.
    let mut docs = Vec::with_capacity(opf.spine_ids.len());
    let mut file_to_index: HashMap<String, usize> = HashMap::new();
    for id in &opf.spine_ids {
        let Some((href, _)) = opf.manifest.get(id) else {
            continue;
        };
        let abs = format!("{opf_base}{}", percent_decode(href));
        file_to_index.entry(basename(&abs)).or_insert(docs.len());
        docs.push(SpineDoc {
            idref: id.clone(),
            href: abs,
            label: None,
        });
    }

    // The declared TOC in its own order, mapped onto those documents. Entries
    // that name nothing in the spine drop out — they cannot speak to an order
    // they are not part of.
    let mut nav_order: Vec<usize> = Vec::new();
    for (label, href) in flatten_declared(&pkg, &opf, &opf_base) {
        let Some(&i) = file_to_index.get(&basename(&href)) else {
            continue;
        };
        if docs[i].label.is_none() {
            docs[i].label = Some(label);
        }
        if !nav_order.contains(&i) {
            nav_order.push(i);
        }
    }

    let descents = nav_order.windows(2).filter(|w| w[0] > w[1]).count();
    let first_out_of_order = nav_order
        .windows(2)
        .find(|w| w[0] > w[1])
        .and_then(|w| docs[w[0]].label.clone());

    let proposed = reorder_to(&nav_order, docs.len());
    let moved = (0..docs.len()).filter(|&i| proposed[i] != i).count();

    Ok(Analysis {
        misordering: Misordering {
            descents,
            moved: if descents == 0 { 0 } else { moved },
            machine_sorted: is_machine_sorted(&opf, &docs),
            first_out_of_order,
        },
        proposed,
        docs,
    })
}

/// The proposed reading order: the documents the navigation names, in *its*
/// order, each still trailed by whichever documents followed it in the spine
/// without being named.
fn reorder_to(nav_order: &[usize], len: usize) -> Vec<usize> {
    let mut named = vec![false; len];
    for &i in nav_order {
        named[i] = true;
    }
    // Each unnamed document attaches to the nearest named one before it.
    let mut trailing: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut prefix = Vec::new();
    let mut anchor: Option<usize> = None;
    for (i, &is_named) in named.iter().enumerate() {
        if is_named {
            anchor = Some(i);
        } else if let Some(a) = anchor {
            trailing.entry(a).or_default().push(i);
        } else {
            prefix.push(i);
        }
    }

    let mut out = prefix;
    for &i in nav_order {
        out.push(i);
        if let Some(rest) = trailing.get(&i) {
            out.extend_from_slice(rest);
        }
    }
    // A named document the navigation lists twice, or a spine the navigation
    // covers only partly, must not cost the book a document.
    debug_assert_eq!(out.len(), len, "a reorder is a permutation");
    out
}

/// Whether the spine is its own manifest in lexicographic order, by `idref` or
/// by filename. Both are checked: a packager sorts whichever string it holds.
fn is_machine_sorted(opf: &OpfData, docs: &[SpineDoc]) -> bool {
    if docs.len() < 3 {
        return false; // too short for an order to mean anything
    }
    let sorted = |mut v: Vec<String>| {
        let original = v.clone();
        v.sort();
        v == original
    };
    let ids: Vec<String> = opf.spine_ids.clone();
    let files: Vec<String> = docs.iter().map(|d| basename(&d.href)).collect();
    sorted(ids) || sorted(files)
}

/// The declared TOC flattened to `(label, absolute href)` in its own order.
pub(crate) fn flatten_declared(
    pkg: &EpubPackage,
    opf: &OpfData,
    opf_base: &str,
) -> Vec<(String, String)> {
    fn walk(entries: &[crate::model::TocEntry], out: &mut Vec<(String, String)>) {
        for e in entries {
            out.push((e.title.clone(), e.href.clone()));
            walk(&e.children, out);
        }
    }
    let mut out = Vec::new();
    walk(&existing_declared_toc(pkg, opf, opf_base), &mut out);
    out
}

// ---------------------------------------------------------------------------
// The OPF write
// ---------------------------------------------------------------------------

fn is_permutation(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut counts: HashMap<&str, isize> = HashMap::new();
    for s in a {
        *counts.entry(s.as_str()).or_default() += 1;
    }
    for s in b {
        match counts.get_mut(s.as_str()) {
            Some(n) => *n -= 1,
            None => return false,
        }
    }
    counts.values().all(|&n| n == 0)
}

/// Rewrite the OPF so its `<itemref>`s read in `order`.
fn permute_itemrefs(opf: &str, order: &[String]) -> io::Result<String> {
    let spine_start = opf
        .find("<spine")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "OPF declares no spine"))?;
    let spine_end = opf[spine_start..]
        .find("</spine>")
        .map(|i| spine_start + i)
        .unwrap_or(opf.len());

    // (start, end, idref) of each `<itemref …>` tag, in document order.
    let mut slots: Vec<(usize, usize, String)> = Vec::new();
    let mut cursor = spine_start;
    while let Some(p) = opf[cursor..spine_end].find("<itemref") {
        let start = cursor + p;
        let Some(gt) = opf[start..spine_end].find('>') else {
            break;
        };
        let end = start + gt + 1;
        let id = attr_value(&opf[start..end], "idref").unwrap_or_default();
        slots.push((start, end, id));
        cursor = end;
    }
    if slots.len() != order.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the OPF holds {} itemrefs but the order names {}",
                slots.len(),
                order.len()
            ),
        ));
    }

    // Which slot supplies each id's tag text. A duplicate idref (invalid, but
    // parseable) hands out its occurrences in the order it declared them.
    let mut by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    for (n, (_, _, id)) in slots.iter().enumerate() {
        by_id.entry(id.as_str()).or_default().push(n);
    }
    let mut taken: HashMap<&str, usize> = HashMap::new();

    let mut out = String::with_capacity(opf.len());
    let mut prev = 0;
    for (n, (start, end, _)) in slots.iter().enumerate() {
        out.push_str(&opf[prev..*start]);
        let want = order[n].as_str();
        let seen = taken.entry(want).or_default();
        let source = by_id
            .get(want)
            .and_then(|v| v.get(*seen))
            .copied()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("the spine has no itemref for {want:?}"),
                )
            })?;
        *seen += 1;
        out.push_str(&opf[slots[source].0..slots[source].1]);
        prev = *end;
    }
    out.push_str(&opf[prev..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unnamed_documents_travel_with_the_document_they_follow() {
        // 0 unnamed (cover), 1..=3 named, 4 unnamed (a plate after 3).
        // Nav order says 3 comes before 1.
        let order = reorder_to(&[3, 1, 2], 5);
        assert_eq!(
            order,
            vec![0, 3, 4, 1, 2],
            "the cover stays at the front and the plate follows document 3"
        );
    }

    #[test]
    fn an_agreeing_book_proposes_itself() {
        assert_eq!(reorder_to(&[0, 1, 2], 3), vec![0, 1, 2]);
    }

    #[test]
    fn permute_keeps_tag_text_and_surroundings() {
        let opf = "<spine toc=\"ncx\">\n  <itemref idref=\"a\" linear=\"no\"/>\n  \
                   <!-- keep me -->\n  <itemref idref=\"b\"/>\n</spine>";
        let out = permute_itemrefs(opf, &["b".into(), "a".into()]).expect("permute");
        assert_eq!(
            out,
            "<spine toc=\"ncx\">\n  <itemref idref=\"b\"/>\n  \
             <!-- keep me -->\n  <itemref idref=\"a\" linear=\"no\"/>\n</spine>",
            "each slot keeps its surroundings and receives the other's tag verbatim"
        );
    }

    #[test]
    fn permutation_check_rejects_a_changed_document_set() {
        let spine = vec!["a".to_string(), "b".to_string()];
        assert!(is_permutation(&spine, &["b".into(), "a".into()]));
        assert!(!is_permutation(&spine, &["a".into(), "a".into()]));
        assert!(!is_permutation(&spine, &["a".into()]));
        assert!(!is_permutation(
            &spine,
            &["a".into(), "b".into(), "c".into()]
        ));
    }
}
