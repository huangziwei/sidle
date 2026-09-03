//! Write a PDF's document outline (`/Outlines`) — the bookmark tree a viewer
//! shows as the table of contents.

use std::io;

use lopdf::{Dictionary, Object, ObjectId};

use super::doc::encode_pdf_string;
use super::edit::PdfPackage;
use crate::formats::pdf::structure::PdfOutlineItem;

/// Deepest outline nesting accepted. Mirrors the reader's own guard
/// ([`crate::import::pdf`] stops at 32) — anything deeper is a malformed tree,
/// not a book.
const MAX_DEPTH: usize = 32;

/// Overwrite a PDF's document outline with `entries`, returning the edited bytes.
pub fn set_toc(pdf_bytes: &[u8], entries: &[PdfOutlineItem]) -> io::Result<Vec<u8>> {
    if entries.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to write an empty table of contents",
        ));
    }
    let mut pkg = PdfPackage::parse(pdf_bytes)?;

    // Page order is the document's own (`get_pages` is keyed by page number), so
    // index i is the i-th page as a reader sees it — the same basis
    // `probe_pdf` hands out in `PdfOutlineItem::page_index`.
    let page_ids: Vec<ObjectId> = pkg.original().get_pages().values().copied().collect();
    if page_ids.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PDF has no pages to point a table of contents at",
        ));
    }
    validate(entries, page_ids.len(), 0)?;

    // Reserve an id per node before emitting anything: the dictionaries are
    // mutually referential (`/Next` needs the sibling that follows it), so every
    // id must exist before any dict can be filled in.
    let nodes = reserve(&mut pkg, entries);

    let (root_id, root_is_new) = outline_root(&mut pkg)?;
    emit(&mut pkg, &nodes, root_id, &page_ids)?;

    let mut root = Dictionary::new();
    root.set("Type", Object::Name(b"Outlines".to_vec()));
    root.set("First", Object::Reference(nodes[0].id));
    root.set("Last", Object::Reference(nodes[nodes.len() - 1].id));
    root.set("Count", Object::Integer(visible_count(entries)));
    *pkg.edit_dict(root_id)? = root;

    if root_is_new {
        let catalog_id = pkg.catalog_id()?;
        pkg.edit_dict(catalog_id)?
            .set("Outlines", Object::Reference(root_id));
    }

    pkg.into_bytes()
}

/// One outline node with its reserved object id, mirroring the input tree.
struct Node<'a> {
    id: ObjectId,
    item: &'a PdfOutlineItem,
    children: Vec<Node<'a>>,
}

/// Reject a tree the writer can't faithfully represent, naming what's wrong.
/// Checked up front so a bad entry fails before anything is staged.
fn validate(items: &[PdfOutlineItem], page_count: usize, depth: usize) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("outline nests deeper than {MAX_DEPTH} levels"),
        ));
    }
    for item in items {
        if item.page_index >= page_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "outline entry {:?} targets page {} but the PDF has {page_count} pages",
                    item.title,
                    item.page_index + 1
                ),
            ));
        }
        validate(&item.children, page_count, depth + 1)?;
    }
    Ok(())
}

/// Allocate an object id for every node, parents before children so ids run in
/// document order (cosmetic, but it makes the appended increment readable).
fn reserve<'a>(pkg: &mut PdfPackage, items: &'a [PdfOutlineItem]) -> Vec<Node<'a>> {
    items
        .iter()
        .map(|item| {
            let id = pkg.add_object(Dictionary::new()); // placeholder; filled by `emit`
            let children = reserve(pkg, &item.children);
            Node { id, item, children }
        })
        .collect()
}

/// The outline root to write into: the catalog's `/Outlines` when it resolves to a
/// dictionary, else a fresh object. The bool means the caller must wire it in.
fn outline_root(pkg: &mut PdfPackage) -> io::Result<(ObjectId, bool)> {
    let catalog_id = pkg.catalog_id()?;
    let existing = pkg
        .original()
        .get_object(catalog_id)
        .and_then(Object::as_dict)
        .ok()
        .and_then(|c| c.get(b"Outlines").ok())
        .and_then(|o| o.as_reference().ok())
        // A catalog can declare an /Outlines that doesn't resolve; treat that as
        // having none rather than trying to edit a phantom.
        .filter(|id| {
            pkg.original()
                .get_object(*id)
                .and_then(Object::as_dict)
                .is_ok()
        });

    match existing {
        Some(id) => Ok((id, false)),
        None => Ok((pkg.add_object(Dictionary::new()), true)),
    }
}

/// Fill in each reserved dictionary and recurse. `parent` is the id every node
/// at this level points back to.
fn emit(
    pkg: &mut PdfPackage,
    nodes: &[Node],
    parent: ObjectId,
    page_ids: &[ObjectId],
) -> io::Result<()> {
    for (i, node) in nodes.iter().enumerate() {
        let mut d = Dictionary::new();
        d.set("Title", encode_pdf_string(&node.item.title));
        d.set("Parent", Object::Reference(parent));
        if i > 0 {
            d.set("Prev", Object::Reference(nodes[i - 1].id));
        }
        if i + 1 < nodes.len() {
            d.set("Next", Object::Reference(nodes[i + 1].id));
        }
        // `validate` has already bounds-checked every page_index.
        d.set(
            "Dest",
            Object::Array(vec![
                Object::Reference(page_ids[node.item.page_index]),
                Object::Name(b"Fit".to_vec()),
            ]),
        );
        if let (Some(first), Some(last)) = (node.children.first(), node.children.last()) {
            d.set("First", Object::Reference(first.id));
            d.set("Last", Object::Reference(last.id));
            // Positive = open. A leaf gets no /Count at all.
            d.set("Count", Object::Integer(visible_count(&node.item.children)));
        }
        *pkg.edit_dict(node.id)? = d;

        emit(pkg, &node.children, node.id, page_ids)?;
    }
    Ok(())
}

/// Number of items visible when every node is open: each child plus all of its
/// descendants. This is what `/Count` means on the root and on an open item.
fn visible_count(items: &[PdfOutlineItem]) -> i64 {
    items.iter().map(|i| 1 + visible_count(&i.children)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::probe_pdf;

    const MINIMAL: &str = "tests/fixtures/minimal.pdf";

    fn minimal() -> Vec<u8> {
        std::fs::read(MINIMAL).expect("read minimal.pdf fixture")
    }

    fn item(title: &str, page: usize, children: Vec<PdfOutlineItem>) -> PdfOutlineItem {
        PdfOutlineItem {
            title: title.to_string(),
            page_index: page,
            children,
        }
    }

    /// Flatten a read-back outline to `(depth, title, page)` for comparison.
    fn flat(items: &[PdfOutlineItem], depth: usize, out: &mut Vec<(usize, String, usize)>) {
        for i in items {
            out.push((depth, i.title.clone(), i.page_index));
            flat(&i.children, depth + 1, out);
        }
    }

    fn read_back(bytes: &[u8]) -> Vec<(usize, String, usize)> {
        let doc = probe_pdf(bytes.to_vec()).expect("probe edited PDF");
        let mut v = Vec::new();
        flat(&doc.outline, 0, &mut v);
        v
    }

    /// A multi-page PDF with **no** outline — the case this exists for.
    /// `Document::new` gives the modern xref-stream layout.
    fn no_toc_pdf(pages: usize) -> Vec<u8> {
        use lopdf::{Document, dictionary};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let kids: Vec<Object> = (0..pages)
            .map(|_| {
                doc.add_object(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                })
                .into()
            })
            .collect();
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => pages as i64,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut out = Vec::new();
        doc.save_to(&mut out).expect("save no-toc pdf");
        out
    }

    /// The flagship: a PDF with no outline gains one, and our independent reader
    /// (`probe_pdf`'s `extract_outline`) resolves every entry to the right page.
    #[test]
    fn adds_a_toc_to_a_pdf_that_has_none() {
        let pdf = no_toc_pdf(10);
        assert!(
            probe_pdf(pdf.clone()).unwrap().outline.is_empty(),
            "precondition: fixture really has no outline"
        );

        let toc = vec![
            item("Chapter 1", 0, vec![]),
            item("Chapter 2", 3, vec![]),
            item("Chapter 3", 7, vec![]),
        ];
        let out = set_toc(&pdf, &toc).expect("set_toc");

        assert!(out.starts_with(&pdf), "append-only");
        assert_eq!(
            read_back(&out),
            vec![
                (0, "Chapter 1".to_string(), 0),
                (0, "Chapter 2".to_string(), 3),
                (0, "Chapter 3".to_string(), 7),
            ]
        );
    }

    /// Nesting round-trips: a Part → chapter tree comes back with its shape and
    /// targets intact.
    #[test]
    fn nested_toc_roundtrips_with_its_shape() {
        let pdf = no_toc_pdf(20);
        let toc = vec![
            item(
                "Part I",
                0,
                vec![item("Chapter 1", 1, vec![]), item("Chapter 2", 5, vec![])],
            ),
            item(
                "Part II",
                10,
                vec![item("Chapter 3", 11, vec![item("Section 3.1", 12, vec![])])],
            ),
        ];
        let out = set_toc(&pdf, &toc).expect("set_toc");

        assert_eq!(
            read_back(&out),
            vec![
                (0, "Part I".to_string(), 0),
                (1, "Chapter 1".to_string(), 1),
                (1, "Chapter 2".to_string(), 5),
                (0, "Part II".to_string(), 10),
                (1, "Chapter 3".to_string(), 11),
                (2, "Section 3.1".to_string(), 12),
            ]
        );
    }

    /// Non-ASCII titles survive — the library is CJK-heavy.
    #[test]
    fn unicode_titles_roundtrip() {
        let pdf = no_toc_pdf(5);
        let toc = vec![
            item("第一章 ー はじめに", 0, vec![]),
            item("第二章 — 「引用」", 2, vec![]),
        ];
        let out = set_toc(&pdf, &toc).expect("set_toc");
        assert_eq!(
            read_back(&out),
            vec![
                (0, "第一章 ー はじめに".to_string(), 0),
                (0, "第二章 — 「引用」".to_string(), 2),
            ]
        );
    }

    /// `/Count` is the number of visible descendants, per §12.3.3.
    #[test]
    fn visible_count_counts_all_open_descendants() {
        assert_eq!(visible_count(&[]), 0);
        assert_eq!(visible_count(&[item("a", 0, vec![])]), 1);
        // One parent + two children = 3 visible.
        assert_eq!(
            visible_count(&[item(
                "p",
                0,
                vec![item("c1", 0, vec![]), item("c2", 0, vec![])]
            )]),
            3
        );
        // Grandchildren count too, when open.
        assert_eq!(
            visible_count(&[item("p", 0, vec![item("c", 0, vec![item("g", 0, vec![])])])]),
            3
        );
    }

    /// The written root declares the right structure.
    #[test]
    fn root_is_wired_into_the_catalog_with_a_correct_count() {
        let pdf = no_toc_pdf(6);
        let toc = vec![
            item("A", 0, vec![item("A.1", 1, vec![])]),
            item("B", 2, vec![]),
        ];
        let out = set_toc(&pdf, &toc).expect("set_toc");

        let doc = super::super::doc::load_pdf(&out).expect("reload");
        let catalog = doc.catalog().expect("catalog");
        let root_ref = catalog
            .get(b"Outlines")
            .expect("catalog declares /Outlines");
        let root = super::super::doc::deref(&doc, root_ref)
            .and_then(|o| o.as_dict().ok())
            .expect("root resolves to a dict");

        assert_eq!(
            root.get(b"Type").and_then(|o| o.as_name()).unwrap(),
            b"Outlines"
        );
        assert_eq!(
            root.get(b"Count").and_then(|o| o.as_i64()).unwrap(),
            3,
            "A + A.1 + B are all visible"
        );
        assert!(root.get(b"First").is_ok() && root.get(b"Last").is_ok());
    }

    /// An existing outline is replaced wholesale, and the root object is reused
    /// so anything else pointing at it stays valid.
    #[test]
    fn replaces_an_existing_outline_reusing_the_root() {
        let pdf = no_toc_pdf(8);
        let first = set_toc(
            &pdf,
            &[item("Old One", 0, vec![]), item("Old Two", 1, vec![])],
        )
        .expect("first set_toc");
        let root_before = {
            let doc = super::super::doc::load_pdf(&first).unwrap();
            doc.catalog()
                .unwrap()
                .get(b"Outlines")
                .unwrap()
                .as_reference()
                .unwrap()
        };

        let second = set_toc(&first, &[item("New Only", 5, vec![])]).expect("second set_toc");
        assert!(second.starts_with(&first), "still append-only");

        assert_eq!(
            read_back(&second),
            vec![(0, "New Only".to_string(), 5)],
            "old entries are gone, not merged"
        );
        let root_after = {
            let doc = super::super::doc::load_pdf(&second).unwrap();
            doc.catalog()
                .unwrap()
                .get(b"Outlines")
                .unwrap()
                .as_reference()
                .unwrap()
        };
        assert_eq!(root_after, root_before, "root object id reused");
    }

    /// Writing a TOC leaves the rest of the document alone.
    #[test]
    fn preserves_pages_and_metadata() {
        let pdf = minimal();
        let before = probe_pdf(pdf.clone()).expect("probe");
        let out = set_toc(&pdf, &[item("Only Chapter", 0, vec![])]).expect("set_toc");
        let after = probe_pdf(out.clone()).expect("probe edited");

        assert_eq!(after.pages.len(), before.pages.len());
        assert_eq!(after.title, before.title, "metadata untouched");
        assert_eq!(after.author, before.author);
        assert_eq!(after.page_labels, before.page_labels);
        assert_eq!(read_back(&out), vec![(0, "Only Chapter".to_string(), 0)]);
    }

    #[test]
    fn rejects_an_empty_toc() {
        let err = set_toc(&minimal(), &[]).expect_err("empty must error");
        assert!(err.to_string().contains("empty"), "{err}");
    }

    /// An entry pointing past the last page is refused, naming the offender —
    /// silently dropping a user-authored entry would be worse.
    #[test]
    fn rejects_an_out_of_range_page() {
        let pdf = no_toc_pdf(3);
        let err = set_toc(&pdf, &[item("Ghost Chapter", 99, vec![])])
            .expect_err("out-of-range must error");
        let msg = err.to_string();
        assert!(
            msg.contains("Ghost Chapter") && msg.contains("3 pages"),
            "{msg}"
        );

        // Nested entries are checked too.
        let err = set_toc(&pdf, &[item("Ok", 0, vec![item("Bad Child", 42, vec![])])])
            .expect_err("nested out-of-range must error");
        assert!(err.to_string().contains("Bad Child"));
    }

    #[test]
    fn rejects_an_absurdly_deep_tree() {
        let pdf = no_toc_pdf(2);
        let mut deep = item("leaf", 0, vec![]);
        for _ in 0..40 {
            deep = item("nest", 0, vec![deep]);
        }
        let err = set_toc(&pdf, &[deep]).expect_err("too deep must error");
        assert!(err.to_string().contains("deeper than"), "{err}");
    }

    #[test]
    fn non_pdf_bytes_error() {
        assert!(set_toc(b"not a pdf", &[item("x", 0, vec![])]).is_err());
    }
}
