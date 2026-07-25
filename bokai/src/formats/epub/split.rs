//! Splitting a collection — a 合本版, a 全集, a boxed set — into the volumes it
//! collects.
//!
//! A collection is N books shipped as one file: each volume keeps its own cover,
//! its own Contents page and its own colophon, and only the container is shared.
//! [`propose_cuts`] finds where one volume ends and the next begins, so the
//! volumes can be published as the separate books they are.
//!
//! Detection reads the book's own navigation and nothing else. The repaired
//! chapter list ([`super::toc_repair::propose_toc`]) already restores the levels
//! a flattened TOC lost, so by the time the proposal is in hand a collection's
//! volumes *are* its top-level entries; what remains is to tell them from the
//! shared front and back matter listed beside them, which is what the evidence
//! below does. A proposal is never written anywhere — the caller confirms the
//! cuts, and renames and renumbers them, first.

use std::collections::HashSet;
use std::io;

use crate::formats::epub::edit::EpubPackage;
use crate::formats::epub::page_shape::single_image_source;
use crate::formats::epub::structure::{dir_of, internal_links, spine_documents, strip_fragment};
use crate::formats::epub::{OpfData, parse_opf, toc_repair};
use crate::model::TocEntry;
use crate::util::{decode_text, extract_xml_encoding};

/// Minimum distinct forward links for a document to read as a volume's own
/// Contents page. Two is enough because the page is only consulted once the
/// book's navigation has already named the entry: a volume of one story plus an
/// afterword is ordinary, and this is not being asked to find Contents pages in
/// a book that has said nothing about where its volumes are.
const MIN_VOLUME_CONTENTS_LINKS: usize = 2;

/// Where one volume of a collection begins.
///
/// A volume's span runs to the next cut, and the last runs to the end of the
/// spine, so the cuts tile everything after the collection's shared front
/// matter.
#[derive(Debug, Clone, PartialEq)]
pub struct Cut {
    /// Spine index of the volume's first document.
    pub spine_index: usize,
    /// Spine documents the volume spans, its own first document included.
    pub documents: usize,
    /// What the collection's own navigation calls this volume.
    pub label: String,
    /// The volume's own cover page, as an absolute zip path — the document the
    /// cut lands on, when that document is a full-bleed image. `None` for a
    /// collection whose volumes open with a Contents page instead.
    pub cover: Option<String>,
    /// The volume's number within the collection. Fractional because publishers
    /// number that way: a 5.5 shipped between volumes 5 and 6 is a real volume
    /// with a real place, not volume 6 under another name.
    pub number: f64,
    /// Where [`Cut::number`] came from.
    pub numbering: Numbering,
}

/// How a volume's number was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Numbering {
    /// The volume's own label states it.
    Label,
    /// The label states none, so the volume takes the number after its
    /// predecessor's.
    Sequence,
}

/// Propose where to cut a collection into volumes, in reading order. Empty for a
/// book that evidences no volumes — which is every ordinary book, and the answer
/// this returns for one.
pub fn propose_cuts(epub_bytes: &[u8]) -> io::Result<Vec<Cut>> {
    let pkg = EpubPackage::parse(epub_bytes)?;
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let opf_str = decode_text(pkg.opf_bytes()?, extract_xml_encoding(pkg.opf_bytes()?));
    let opf = parse_opf(&opf_str).map_err(io::Error::other)?;
    let toc = toc_repair::propose_from_pkg(&pkg, &opf, &opf_base, &opf_str);
    Ok(cuts(&pkg, &opf, &opf_base, &toc))
}

/// A candidate volume start: a top-level entry of the repaired chapter list,
/// resolved to the spine document it opens.
struct Candidate<'a> {
    spine_index: usize,
    label: &'a str,
}

fn cuts(pkg: &EpubPackage, opf: &OpfData, opf_base: &str, toc: &[TocEntry]) -> Vec<Cut> {
    let spine = spine_documents(opf, opf_base);
    let spine_files: HashSet<&str> = spine.iter().map(|(_, b)| b.as_str()).collect();
    let candidates = candidates(toc, &spine);
    let starts = volume_starts(pkg, &spine, &spine_files, &candidates);
    if starts.len() < 2 {
        // One volume is not a collection, and neither is none.
        return Vec::new();
    }

    let mut cuts: Vec<Cut> = Vec::new();
    for (n, &start) in starts.iter().enumerate() {
        let candidate = &candidates[start];
        let span = span(spine.len(), &candidates, &starts, n);
        let (number, numbering) = number_after(cuts.last(), candidate.label, n);
        cuts.push(Cut {
            spine_index: candidate.spine_index,
            documents: span.len(),
            label: candidate.label.to_string(),
            cover: cover_page(pkg, &spine[candidate.spine_index].0),
            number,
            numbering,
        });
    }
    cuts
}

/// The spine range the `n`th of `starts` covers: from its own document up to
/// the next start's, and to the end of the book for the last.
fn span(
    spine_len: usize,
    candidates: &[Candidate<'_>],
    starts: &[usize],
    n: usize,
) -> std::ops::Range<usize> {
    let from = candidates[starts[n]].spine_index;
    let to = starts
        .get(n + 1)
        .map(|&next| candidates[next].spine_index)
        .unwrap_or(spine_len);
    from..to
}

/// The top-level chapter-list entries that open a spine document, one per
/// document, **in reading order**. Never the book's own first document:
/// whatever a collection opens with belongs to the collection, not to a volume
/// inside it.
///
/// Reading order is the spine's, not the chapter list's: a declared TOC is free
/// to name an entry out of order (and some do), while a volume is a contiguous
/// run of the book. Where two entries name one document the first in the
/// chapter list keeps the naming.
fn candidates<'a>(toc: &'a [TocEntry], spine: &[(String, String)]) -> Vec<Candidate<'a>> {
    let mut out: Vec<Candidate<'a>> = Vec::new();
    for entry in toc {
        let doc = strip_fragment(&entry.href);
        let Some(spine_index) = spine.iter().position(|(abs, _)| abs == doc) else {
            continue;
        };
        if spine_index == 0 || out.iter().any(|c| c.spine_index == spine_index) {
            continue;
        }
        out.push(Candidate {
            spine_index,
            label: &entry.title,
        });
    }
    out.sort_by_key(|c| c.spine_index);
    out
}

/// Which candidates are volume starts, as indices into `candidates`.
///
/// A volume announces itself with its own front matter, and the two forms that
/// takes are not equally telling:
///
/// - **Its own cover** — a full-bleed image page. Strong, because in an ordinary
///   book nothing but a cover looks like that; but the collection as a whole
///   still has to hold up ([`reads_as_a_collection`]), since plenty of books are
///   pictures from end to end.
/// - **Its own Contents page** — a page listing what follows it. Weaker, because
///   any chapter carrying a few cross-references looks the same from outside, so
///   it is held to a stricter test ([`contents_page_starts`]) and read only
///   where the cover signal found nothing.
fn volume_starts(
    pkg: &EpubPackage,
    spine: &[(String, String)],
    spine_files: &HashSet<&str>,
    candidates: &[Candidate<'_>],
) -> Vec<usize> {
    let covers: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| cover_page(pkg, &spine[c.spine_index].0).is_some())
        .map(|(n, _)| n)
        .collect();
    if reads_as_a_collection(pkg, spine, spine_files, candidates, &covers) {
        return covers;
    }
    contents_page_starts(pkg, spine, spine_files, candidates)
}

/// Whether cover-shaped starts really divide a collection: most of them own a
/// Contents page, somewhere in the span they open.
///
/// Whether a book is a collection is one question about the book, not one per
/// candidate, and asking it per candidate gets it wrong in both directions. A
/// fixed-layout title or an illustrated reference is a full-bleed image at every
/// entry in its chapter list, and *none* of those spans names its own contents —
/// so the shape means nothing there. A real collection's volumes do name theirs,
/// but not always every one of them: a two-story volume needs no Contents page,
/// and a publisher that draws one as a picture leaves no links to find. Once the
/// collection holds up, that volume is a volume like its neighbours.
fn reads_as_a_collection(
    pkg: &EpubPackage,
    spine: &[(String, String)],
    spine_files: &HashSet<&str>,
    candidates: &[Candidate<'_>],
    starts: &[usize],
) -> bool {
    let owning = (0..starts.len())
        .filter(|&n| {
            span(spine.len(), candidates, starts, n).any(|i| {
                linked_documents(pkg, spine, spine_files, i).len() >= MIN_VOLUME_CONTENTS_LINKS
            })
        })
        .count();
    owning * 2 > starts.len()
}

/// The candidates whose own document is a Contents page **for its own span** —
/// every document it links to falls between it and the next entry the chapter
/// list names.
///
/// Read literally, "links to several other documents" describes any chapter with
/// endnotes or cross-references, which is most non-fiction. What makes a page a
/// volume's Contents page is not that it links but *where*: it enumerates the
/// volume it opens and reaches nothing outside it. That also settles the one
/// page most like it — the collection's own Contents page reaches the volume
/// starts, which lie well past the next entry.
fn contents_page_starts(
    pkg: &EpubPackage,
    spine: &[(String, String)],
    spine_files: &HashSet<&str>,
    candidates: &[Candidate<'_>],
) -> Vec<usize> {
    candidates
        .iter()
        .enumerate()
        .filter(|&(n, c)| {
            let next = candidates
                .get(n + 1)
                .map(|c| c.spine_index)
                .unwrap_or(spine.len());
            let own: HashSet<&str> = spine[c.spine_index..next]
                .iter()
                .map(|(abs, _)| abs.as_str())
                .collect();
            let targets = linked_documents(pkg, spine, spine_files, c.spine_index);
            targets.len() >= MIN_VOLUME_CONTENTS_LINKS
                && targets.iter().all(|doc| own.contains(doc.as_str()))
        })
        .map(|(n, _)| n)
        .collect()
}

/// The distinct spine documents the document at `spine_index` links to, as
/// absolute zip paths. Empty for anything that is not a Contents page.
fn linked_documents(
    pkg: &EpubPackage,
    spine: &[(String, String)],
    spine_files: &HashSet<&str>,
    spine_index: usize,
) -> HashSet<String> {
    let (abs, base) = &spine[spine_index];
    let Some(bytes) = pkg.get(abs) else {
        return HashSet::new();
    };
    let xhtml = decode_text(bytes, extract_xml_encoding(bytes));
    internal_links(&xhtml, &dir_of(abs), spine_files, base)
        .iter()
        .map(|(_, href)| strip_fragment(href).to_string())
        .collect()
}

/// The document's own zip path when it is a full-bleed image page — a volume's
/// cover — and `None` when it is anything else.
fn cover_page(pkg: &EpubPackage, abs: &str) -> Option<String> {
    let bytes = pkg.get(abs)?;
    let xhtml = decode_text(bytes, extract_xml_encoding(bytes));
    single_image_source(&xhtml)?;
    Some(abs.to_string())
}

// ---------------------------------------------------------------------------
// Volume numbers
// ---------------------------------------------------------------------------

/// The number to give the volume `label` names, following `previous`.
///
/// A label is believed only where it continues the numbering: an index that goes
/// backwards is not this volume's place in the series, it is a number that
/// happens to be in its title. A collection that runs its main line to
/// twenty-seven and then ships side stories numbered from one again is counting
/// two different things, and the volume's place in the collection is the one
/// being asked for.
fn number_after(previous: Option<&Cut>, label: &str, index: usize) -> (f64, Numbering) {
    let previous = previous.map(|c| c.number);
    match volume_number(label) {
        Some(n) if previous.is_none_or(|p| n > p) => (n, Numbering::Label),
        // Counting on from the volume before rather than from the position, so
        // a collection that starts at volume ten keeps counting from ten.
        _ => (
            previous.map_or(index as f64 + 1.0, |p| p + 1.0),
            Numbering::Sequence,
        ),
    }
}

/// The volume number a label states, or `None` when it states none.
///
/// Two forms, and only two, because only these two *number* rather than name: a
/// `第…巻` counter construction, and a number the label ends on. A numeral
/// anywhere else belongs to the title — a book named for eight graves is the
/// eighth of nothing, and a subtitled side story numbers the side stories rather
/// than the collection.
fn volume_number(label: &str) -> Option<f64> {
    counted_number(label).or_else(|| trailing_number(label))
}

/// The counters a publisher numbers volumes with, in both the Japanese and the
/// Chinese spelling. 章 and 話 are deliberately absent: they count chapters and
/// episodes, which are what a volume *contains*.
const VOLUME_COUNTERS: [char; 6] = ['巻', '卷', '部', '冊', '册', '集'];

/// `第` + numerals + a volume counter, as in 第一部 / 第二十七部 / 第３巻.
fn counted_number(label: &str) -> Option<f64> {
    let mut rest = label;
    while let Some((_, after)) = rest.split_once('第') {
        rest = after;
        let digits: String = after
            .chars()
            .take_while(|c| is_numeral(*c) || kanji_digit(*c).is_some() || kanji_unit(*c).is_some())
            .collect();
        let counter = after[digits.len()..].chars().next();
        if !digits.is_empty() && counter.is_some_and(|c| VOLUME_COUNTERS.contains(&c)) {
            return read_numerals(&digits);
        }
    }
    None
}

/// A number the label ends on, with nothing after it but closing punctuation and
/// at most a counter — `…2`, `…10.5`, `…(12)`, `…１`, `…3巻`.
fn trailing_number(label: &str) -> Option<f64> {
    let tail = label.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, ')' | '）' | ']' | '】' | '〉' | '》' | '>')
    });
    let tail = match tail.chars().next_back() {
        Some(c) if VOLUME_COUNTERS.contains(&c) => &tail[..tail.len() - c.len_utf8()],
        _ => tail,
    };
    let start = tail
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_numeral(c))
        .last()
        .map(|(i, _)| i)?;
    read_numerals(&tail[start..])
}

/// ASCII and full-width digits and decimal points — what a number is written
/// with when it is written as a number.
fn is_numeral(c: char) -> bool {
    c.is_ascii_digit() || ('０'..='９').contains(&c) || c == '.' || c == '．'
}

fn kanji_digit(c: char) -> Option<f64> {
    "〇一二三四五六七八九"
        .chars()
        .position(|d| d == c)
        .map(|n| n as f64)
}

fn kanji_unit(c: char) -> Option<f64> {
    match c {
        '十' => Some(10.0),
        '百' => Some(100.0),
        '千' => Some(1000.0),
        _ => None,
    }
}

/// Read a run of numerals as a number, whether written with digits (`27`, `５`,
/// `5.5`) or with kanji (`二十七`). A run that mixes the two, or that is not a
/// number at all, comes back `None`.
fn read_numerals(s: &str) -> Option<f64> {
    if s.chars().all(is_numeral) {
        let plain: String = s
            .chars()
            .map(|c| match c {
                '０'..='９' => char::from(b'0' + (c as u32 - '０' as u32) as u8),
                '．' => '.',
                c => c,
            })
            .collect();
        return plain.parse().ok();
    }
    // 二十七 = 2×10 + 7, 十 = 10, 百二十三 = 1×100 + 2×10 + 3.
    let mut total = 0.0;
    let mut digit: Option<f64> = None;
    for c in s.chars() {
        if let Some(d) = kanji_digit(c) {
            digit = Some(d);
        } else if let Some(unit) = kanji_unit(c) {
            total += digit.unwrap_or(1.0) * unit;
            digit = None;
        } else {
            return None;
        }
    }
    Some(total + digit.unwrap_or(0.0))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    /// A document of a synthetic book: `(filename, body markup, nav label)`. A
    /// document with no label is in the spine but not in the chapter list.
    type Doc<'a> = (&'a str, String, Option<&'a str>);

    fn image_page(src: &str) -> String {
        format!(r#"<img src="{src}" alt=""/>"#)
    }

    fn contents_page(targets: &[&str]) -> String {
        targets
            .iter()
            .map(|t| format!(r#"<p><a href="{t}">{t}</a></p>"#))
            .collect()
    }

    /// Zip the documents into an EPUB whose nav doc lists the labelled ones, in
    /// spine order.
    fn epub(docs: &[Doc<'_>]) -> Vec<u8> {
        let rows: String = docs
            .iter()
            .filter_map(|(name, _, label)| {
                Some(format!(r#"<li><a href="{name}">{}</a></li>"#, (*label)?))
            })
            .collect();
        epub_with_nav(docs, &rows)
    }

    /// As [`epub`], but with the chapter list written out by hand — for a nav
    /// doc whose order or shape the per-document labels cannot express.
    fn epub_with_nav(docs: &[Doc<'_>], rows: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let mut add = |name: &str, body: &str| {
                zip.start_file(name, stored).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            };
            add("mimetype", "application/epub+zip");
            add(
                "META-INF/container.xml",
                r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            );
            let mut manifest = String::from(
                r#"<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>"#,
            );
            let mut spine = String::new();
            for (n, (name, body, _)) in docs.iter().enumerate() {
                manifest.push_str(&format!(
                    r#"<item id="d{n}" href="{name}" media-type="application/xhtml+xml"/>"#
                ));
                spine.push_str(&format!(r#"<itemref idref="d{n}"/>"#));
                add(
                    &format!("OEBPS/{name}"),
                    &format!(
                        r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>{name}</title></head><body>{body}</body></html>"#
                    ),
                );
            }
            add(
                "OEBPS/content.opf",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Collected</dc:title><dc:language>ja</dc:language><dc:identifier id="uid">urn:uuid:collected</dc:identifier></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
                ),
            );
            add(
                "OEBPS/nav.xhtml",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Contents</title></head><body><nav epub:type="toc"><ol>{rows}</ol></nav></body></html>"#
                ),
            );
            zip.finish().unwrap();
        }
        buf
    }

    /// The documents of one volume: its cover, its own Contents page, and the
    /// chapters that page names.
    fn volume(v: usize) -> Vec<Doc<'static>> {
        const COVERS: [&str; 3] = ["v1.xhtml", "v2.xhtml", "v3.xhtml"];
        const TOCS: [&str; 3] = ["v1toc.xhtml", "v2toc.xhtml", "v3toc.xhtml"];
        const CHAPTERS: [[&str; 2]; 3] = [
            ["v1c1.xhtml", "v1c2.xhtml"],
            ["v2c1.xhtml", "v2c2.xhtml"],
            ["v3c1.xhtml", "v3c2.xhtml"],
        ];
        let mut docs: Vec<Doc<'static>> = vec![
            (COVERS[v - 1], image_page(&format!("v{v}.jpg")), None),
            (TOCS[v - 1], contents_page(&CHAPTERS[v - 1]), None),
        ];
        for (n, name) in CHAPTERS[v - 1].iter().enumerate() {
            docs.push((name, format!("<h1>第{}章</h1><p>text</p>", n + 1), None));
        }
        docs
    }

    /// A collection whose volumes open with their own cover: three volumes of
    /// four documents each, wrapped in the shared cover / Contents / colophon
    /// every collection carries.
    fn collection_documents() -> Vec<Doc<'static>> {
        let mut docs: Vec<Doc<'static>> = vec![
            ("cover.xhtml", image_page("cover.jpg"), Some("表紙")),
            (
                "toc.xhtml",
                contents_page(&["v1.xhtml", "v2.xhtml", "v3.xhtml"]),
                Some("目次"),
            ),
        ];
        for (v, label) in [(1, "物語"), (2, "物語2"), (3, "物語3")] {
            let mut volume = volume(v);
            volume[0].2 = Some(label);
            docs.extend(volume);
        }
        docs.push(("colophon.xhtml", "<p>奥付</p>".into(), Some("奥付")));
        docs
    }

    fn collection_with_covers() -> Vec<u8> {
        epub(&collection_documents())
    }

    #[test]
    fn a_collection_cuts_at_the_volumes_its_covers_announce() {
        let cuts = propose_cuts(&collection_with_covers()).expect("propose");
        assert_eq!(
            cuts.iter()
                .map(|c| (c.spine_index, c.documents, c.label.as_str(), c.number))
                .collect::<Vec<_>>(),
            [
                (2, 4, "物語", 1.0),
                (6, 4, "物語2", 2.0),
                (10, 5, "物語3", 3.0),
            ],
            "the shared front matter is left out and the last cut takes the colophon"
        );
        assert_eq!(cuts[0].cover.as_deref(), Some("OEBPS/v1.xhtml"));
        // The spans tile: everything from the first cut to the end of the book.
        assert_eq!(
            cuts.iter().map(|c| c.documents).sum::<usize>(),
            15 - cuts[0].spine_index
        );
    }

    /// A publisher's own nesting is kept exactly as declared, order included —
    /// and some declare an entry out of the book's order. A volume is still a
    /// contiguous run of the book, so the spine's order is the one that decides
    /// where one ends; reading the chapter list's would give a volume ending
    /// before it starts.
    #[test]
    fn a_chapter_list_out_of_order_is_read_in_the_books_order() {
        let docs = collection_documents();
        // Volume 3 declared before volume 1, each with a child so the declared
        // nesting survives into the proposal untouched.
        let rows = concat!(
            r#"<li><a href="cover.xhtml">表紙</a></li>"#,
            r#"<li><a href="toc.xhtml">目次</a><ol><li><a href="colophon.xhtml">奥付</a></li></ol></li>"#,
            r#"<li><a href="v3.xhtml">物語3</a><ol><li><a href="v3c1.xhtml">一</a></li></ol></li>"#,
            r#"<li><a href="v1.xhtml">物語</a><ol><li><a href="v1c1.xhtml">一</a></li></ol></li>"#,
            r#"<li><a href="v2.xhtml">物語2</a><ol><li><a href="v2c1.xhtml">一</a></li></ol></li>"#,
        );
        let cuts = propose_cuts(&epub_with_nav(&docs, rows)).expect("propose");
        assert_eq!(
            cuts.iter()
                .map(|c| (c.spine_index, c.documents, c.label.as_str(), c.number))
                .collect::<Vec<_>>(),
            [
                (2, 4, "物語", 1.0),
                (6, 4, "物語2", 2.0),
                (10, 5, "物語3", 3.0),
            ]
        );
    }

    /// A book whose chapters open with a full-page title image looks, page by
    /// page, exactly like a collection whose volumes open with a cover. What it
    /// does not have is volumes that name their own contents.
    #[test]
    fn chapters_that_open_with_a_picture_are_not_volumes() {
        let mut docs: Vec<Doc<'static>> = vec![
            ("cover.xhtml", image_page("cover.jpg"), Some("表紙")),
            (
                "toc.xhtml",
                contents_page(&["t1.xhtml", "t2.xhtml", "t3.xhtml"]),
                Some("目次"),
            ),
        ];
        for n in 1..=3 {
            docs.push((
                ["t1.xhtml", "t2.xhtml", "t3.xhtml"][n - 1],
                image_page(&format!("title{n}.jpg")),
                Some(["一章", "二章", "三章"][n - 1]),
            ));
            docs.push((
                ["c1.xhtml", "c2.xhtml", "c3.xhtml"][n - 1],
                "<p>text</p>".into(),
                None,
            ));
        }
        assert!(propose_cuts(&epub(&docs)).expect("propose").is_empty());
    }

    /// Volumes that open with a Contents page instead of a cover — nothing in
    /// the book is a full-bleed image, so the weaker signal is all there is.
    /// The collection's own Contents page must not become a volume.
    #[test]
    fn a_collection_with_no_covers_cuts_at_the_contents_pages_instead() {
        let mut docs: Vec<Doc<'static>> = vec![(
            "toc.xhtml",
            contents_page(&["v1.xhtml", "v2.xhtml", "v3.xhtml", "colophon.xhtml"]),
            Some("总目录"),
        )];
        for (v, label) in [(1, "文集•第一卷"), (2, "文集•第二卷"), (3, "文集•第三卷")]
        {
            let chapters: Vec<&str> = [
                "v1c1.xhtml",
                "v1c2.xhtml",
                "v2c1.xhtml",
                "v2c2.xhtml",
                "v3c1.xhtml",
                "v3c2.xhtml",
            ][(v - 1) * 2..(v - 1) * 2 + 2]
                .to_vec();
            docs.push((
                ["v1.xhtml", "v2.xhtml", "v3.xhtml"][v - 1],
                contents_page(&chapters),
                Some(label),
            ));
            for c in chapters {
                docs.push((c, "<p>text</p>".into(), None));
            }
        }
        docs.push(("colophon.xhtml", "<p>奥付</p>".into(), Some("奥付")));

        let cuts = propose_cuts(&epub(&docs)).expect("propose");
        assert_eq!(
            cuts.iter()
                .map(|c| (c.label.as_str(), c.number, c.cover.is_some()))
                .collect::<Vec<_>>(),
            [
                ("文集•第一卷", 1.0, false),
                ("文集•第二卷", 2.0, false),
                ("文集•第三卷", 3.0, false),
            ],
            "the page listing the volumes is the collection's, not a volume's"
        );
    }

    /// An ordinary book is not a collection, and the answer for one is nothing
    /// at all — not its chapters, and not the one illustration plate that has
    /// the shape of a cover.
    #[test]
    fn an_ordinary_book_proposes_no_cuts() {
        let mut docs: Vec<Doc<'static>> = vec![
            ("cover.xhtml", image_page("cover.jpg"), Some("表紙")),
            (
                "toc.xhtml",
                contents_page(&["c1.xhtml", "c2.xhtml", "c3.xhtml"]),
                Some("目次"),
            ),
            ("plate.xhtml", image_page("plate.jpg"), Some("口絵")),
        ];
        for (n, name) in ["c1.xhtml", "c2.xhtml", "c3.xhtml"].iter().enumerate() {
            docs.push((
                name,
                format!("<h1>第{}章</h1><p>text</p>", n + 1),
                Some("章"),
            ));
        }
        assert!(propose_cuts(&epub(&docs)).expect("propose").is_empty());
    }

    #[test]
    fn a_label_is_read_for_a_number_only_where_it_states_one() {
        // Counter constructions, in both spellings and both digit sets.
        assert_eq!(volume_number("物語　第一部　序"), Some(1.0));
        assert_eq!(volume_number("物語　第二十七部　終"), Some(27.0));
        assert_eq!(volume_number("文集•第十九卷"), Some(19.0));
        assert_eq!(volume_number("第３巻"), Some(3.0));

        // Numbers a label ends on, however they are punctuated.
        assert_eq!(volume_number("物語2"), Some(2.0));
        assert_eq!(volume_number("物語10.5"), Some(10.5));
        assert_eq!(volume_number("物語(12)"), Some(12.0));
        assert_eq!(volume_number("議事録１"), Some(1.0));
        assert_eq!(volume_number("物語 3巻"), Some(3.0));

        // Numerals that name rather than number.
        assert_eq!(volume_number("八つ墓の村"), None);
        assert_eq!(volume_number("三つ首の塔"), None);
        assert_eq!(volume_number("物語　外伝１　副題つきの一編"), None);
        assert_eq!(volume_number("物語　議事録 上"), None);
        assert_eq!(volume_number("【合本版】物語 全13巻 表紙"), None);
    }

    /// A numbering is a numbering: it only ever goes up.
    #[test]
    fn a_label_that_does_not_continue_the_numbering_is_not_believed() {
        let after = |previous: f64, label: &str, index: usize| {
            let cut = Cut {
                spine_index: 0,
                documents: 1,
                label: String::new(),
                cover: None,
                number: previous,
                numbering: Numbering::Label,
            };
            number_after(Some(&cut), label, index)
        };
        // A side story numbered 1, shipped as the twenty-ninth volume, is the
        // twenty-ninth volume.
        assert_eq!(after(28.0, "物語　外伝１", 28), (29.0, Numbering::Sequence));
        // A label that does continue the numbering is taken at its word,
        // fractions included.
        assert_eq!(after(5.0, "物語5.5", 5), (5.5, Numbering::Label));
        // A collection that starts at ten keeps counting from ten.
        assert_eq!(after(10.0, "無題", 1), (11.0, Numbering::Sequence));
        // With nothing before it, an unnumbered volume takes its position.
        assert_eq!(number_after(None, "無題", 0), (1.0, Numbering::Sequence));
    }
}
