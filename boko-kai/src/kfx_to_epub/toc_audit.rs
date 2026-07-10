//! KFX TOC quality check.
//!
//! Compares the KFX's `nav_container` TOC (the *suspect* — it drives the Kindle
//! "Go To" menu, boko's EPUB nav, and the Sidle reader sidebar) against the
//! book's own in-book Contents evidence (the *ground truth*): the densest
//! cluster of internal `link_to` ($179) anchors, i.e. the chapter links on the
//! 目次 / Contents page. When a `toc`-type landmark is present the publisher has
//! named the Contents page explicitly, so its cluster is used directly.
//!
//! This is deliberately KFX-native: it never inspects any derived (EPUB) output.
//! A converted EPUB is downstream of the KFX and gets no say in whether the KFX
//! is well-formed.
//!
//! The check only *flags* — it never mutates a book. A flagged book is a
//! candidate for a human to confirm before its TOC is synthesised.

use std::collections::HashSet;

use crate::kfx::container::get_field;
use crate::kfx::ion::IonValue;
use crate::kfx::symbols::KfxSymbol;

use std::collections::HashMap;

use super::loader::BookData;
use super::navigation::{AnchorTable, NavPoint, extract_anchors, extract_toc};
use super::properties;

/// Result of auditing one book's TOC. All counts are derived from the KFX only.
#[derive(Debug, Clone)]
pub struct TocAudit {
    /// Total nav_container TOC entries (recursive, incl. nested).
    pub nav_count: usize,
    /// Flattened nav TOC entry labels.
    pub nav_labels: Vec<String>,
    /// Nav entries whose label is not front-matter/boilerplate — the real
    /// chapter entries the reader can navigate by.
    pub nav_chapters: usize,
    /// Nav TOC is present but every entry is front-matter (表紙/目次/奥付/…).
    pub fm_only: bool,
    /// Distinct internal chapter links on the in-book Contents page (the densest
    /// `link_to` cluster, or the `toc`-landmark page when marked).
    pub contents_links: usize,
    /// A few chapter-link labels from that page, for the review report.
    pub contents_sample: Vec<String>,
    /// Heading-styled elements ($760 treat_as_title / `$761` layout_hints
    /// "heading") in the content — the fallback chapter-list evidence for books
    /// that carry no `link_to` Contents page (chapters are un-anchored headings).
    pub headings: usize,
    /// Storylines whose first text is a bare chapter marker (a standalone number,
    /// 第N章/部/話, or "Chapter N") — the last-resort evidence for books whose
    /// chapters are neither linked nor heading-styled, only numbered at section
    /// starts (e.g. some Hayakawa mystery editions).
    pub section_heads: usize,
    /// The book carries a `toc`-type landmark naming its Contents page.
    pub has_toc_landmark: bool,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nav TOC already lists chapters (or no stronger in-book evidence exists).
    Ok,
    /// Nav TOC is chapterless but the book's Contents page lists many chapters —
    /// the nav TOC is deficient. Candidate for confirm-then-fix.
    Suspect,
    /// Nav TOC is chapterless and there's no machine-readable in-book chapter
    /// list either (no anchor cluster). May be a genuinely flat book, or its
    /// chapters are un-anchored headings (needs the heading fallback). Left
    /// alone until a human looks.
    Sparse,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Ok => "OK",
            Verdict::Suspect => "SUSPECT",
            Verdict::Sparse => "SPARSE",
        }
    }
}

/// Minimum in-book chapter links for the evidence to count as a real Contents
/// page (below this, a stray forward link or two is just noise).
const MIN_EVIDENCE: usize = 5;

/// A nav TOC with more real chapter entries than this is considered healthy and
/// is never flagged, however many internal links the body carries (footnote /
/// index / cross-reference links inflate the raw link count but say nothing
/// about the TOC).
const MAX_NAV_CHAPTERS_TO_FLAG: usize = 2;

/// Audit one already-loaded book.
pub fn audit(book: &BookData) -> TocAudit {
    let anchors = extract_anchors(book);
    let empty_files = std::collections::HashMap::new();
    let toc = extract_toc(book, &empty_files, &AnchorTable::default());

    let mut nav_labels = Vec::new();
    flatten_labels(&toc, &mut nav_labels);
    let nav_count = nav_labels.len();
    let nav_chapters = nav_labels.iter().filter(|l| !is_front_matter(l)).count();
    let fm_only = nav_count > 0 && nav_chapters == 0;

    let (contents_links, contents_sample) = in_book_contents(book, &anchors);
    let headings = count_headings(book);
    let section_heads = count_section_heads(book);
    let has_toc_landmark = toc_landmark_eid(book).is_some();

    // Ground-truth chapter count = the strongest of the three in-book signals.
    let evidence = contents_links.max(headings).max(section_heads);
    let verdict = if nav_chapters > MAX_NAV_CHAPTERS_TO_FLAG {
        Verdict::Ok
    } else if evidence >= MIN_EVIDENCE && evidence > nav_count {
        Verdict::Suspect
    } else {
        Verdict::Sparse
    };

    TocAudit {
        nav_count,
        nav_labels,
        nav_chapters,
        fm_only,
        contents_links,
        contents_sample,
        headings,
        section_heads,
        has_toc_landmark,
        verdict,
    }
}

fn flatten_labels(points: &[NavPoint], out: &mut Vec<String>) {
    for p in points {
        out.push(p.label.clone());
        flatten_labels(&p.children, out);
    }
}

/// The in-book Contents page's chapter links: distinct internal `link_to`
/// destinations in the storyline that carries the most of them. When a `toc`
/// landmark names a page, the storyline containing that page's eid wins even if
/// another storyline has more links (e.g. a footnote-dense chapter). Returns the
/// count and a few source-text samples for the report.
fn in_book_contents(book: &BookData, anchors: &AnchorTable) -> (usize, Vec<String>) {
    let landmark_eid = toc_landmark_eid(book);
    let Some(storylines) = book.by_type.get(&(KfxSymbol::Storyline as u64)) else {
        return (0, Vec::new());
    };

    let mut best_count = 0usize;
    let mut best_sample: Vec<String> = Vec::new();
    let mut landmark_hit: Option<(usize, Vec<String>)> = None;

    for storyline in storylines.values() {
        let mut links = Vec::new();
        collect_link_to(storyline, book, &mut links);
        let mut dests: HashSet<(i64, i64)> = HashSet::new();
        for name in &links {
            if let Some(&pos) = anchors.name_to_position.get(name) {
                dests.insert(pos);
            }
        }
        // A couple of anchor names as a human-readable sample.
        let sample: Vec<String> = links.iter().take(6).cloned().collect();

        let count = dests.len();
        if count > best_count {
            best_count = count;
            best_sample = sample.clone();
        }
        // If a toc landmark named a page, prefer the storyline that actually
        // contains that eid (disambiguates against a footnote-dense chapter that
        // happens to hold more links).
        if let Some(le) = landmark_eid
            && count >= MIN_EVIDENCE
        {
            let mut ids = HashSet::new();
            collect_ids(storyline, &mut ids);
            if ids.contains(&le) {
                landmark_hit = Some((count, sample));
            }
        }
    }

    landmark_hit.unwrap_or((best_count, best_sample))
}

/// Count heading-styled elements across all storylines — boko's `<hN>`
/// promotion criterion (a `$760 treat_as_title` style, or a `$761 layout_hints`
/// list containing "heading"). This is the ground-truth chapter list for books
/// whose chapters are un-anchored styled headings rather than Contents-page
/// links. Style→heading resolution is memoised per style name.
fn count_headings(book: &BookData) -> usize {
    let Some(storylines) = book.by_type.get(&(KfxSymbol::Storyline as u64)) else {
        return 0;
    };
    let mut memo: HashMap<String, bool> = HashMap::new();
    let mut n = 0usize;
    for sv in storylines.values() {
        count_headings_in(sv, book, &mut memo, &mut n);
    }
    n
}

fn count_headings_in(
    value: &IonValue,
    book: &BookData,
    memo: &mut HashMap<String, bool>,
    n: &mut usize,
) {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            let mut is_heading = false;
            if let Some(name) =
                get_field(fields, KfxSymbol::Style as u64).and_then(|v| book.symbols.text_of(v))
            {
                is_heading = *memo.entry(name.to_string()).or_insert_with(|| {
                    properties::style_layout_hints_for(name, book)
                        .0
                        .iter()
                        .any(|h| h == "heading")
                });
            }
            if !is_heading {
                let (hints, _) =
                    properties::layout_hints_from_element_fields(fields, &book.symbols);
                is_heading = hints.iter().any(|h| h == "heading");
            }
            if is_heading {
                *n += 1;
            }
            for (_, v) in fields {
                count_headings_in(v, book, memo, n);
            }
        }
        IonValue::List(items) => {
            for it in items {
                count_headings_in(it, book, memo, n);
            }
        }
        _ => {}
    }
}

/// Count storylines whose first text block is a bare chapter marker. Some
/// editions (e.g. Hayakawa mysteries) split each chapter into its own storyline
/// and open it with just the chapter number — no anchor, no heading style — so
/// this is the only machine-readable trace of the chapter list. A book with no
/// chapter divisions (a continuous novella) opens each storyline with prose, so
/// this stays ~0.
fn count_section_heads(book: &BookData) -> usize {
    let Some(storylines) = book.by_type.get(&(KfxSymbol::Storyline as u64)) else {
        return 0;
    };
    storylines
        .values()
        .filter(|s| first_text(s, book).is_some_and(|t| is_chapter_marker(&t)))
        .count()
}

/// The first non-empty text of a value tree (depth-first). Resolves `$145
/// content` via the same helper the emitter uses.
fn first_text(value: &IonValue, book: &BookData) -> Option<String> {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            if let Some(c) = get_field(fields, KfxSymbol::Content as u64) {
                let t = super::content::resolve_content_text(c, book);
                if !t.trim().is_empty() {
                    return Some(t);
                }
            }
            for (_, v) in fields {
                if let Some(t) = first_text(v, book) {
                    return Some(t);
                }
            }
            None
        }
        IonValue::List(items) => items.iter().find_map(|it| first_text(it, book)),
        _ => None,
    }
}

/// Whether a short line reads as a standalone chapter marker: a bare number
/// (`1`, `12`), a Japanese `第N章/部/話/節`, or an English `Chapter N`. Kept
/// tight so prose first-lines and dropcaps (a single letter) don't match.
fn is_chapter_marker(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return false;
    }
    // Bare number (Western or full-width digits), up to 4 digits.
    let digits: String = t
        .chars()
        .map(|c| match c {
            '０'..='９' => char::from(b'0' + (c as u32 - '０' as u32) as u8),
            other => other,
        })
        .collect();
    if digits.len() <= 4 && digits.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    let n = t.chars().count();
    // 第…章 / 第…部 / 第…話 / 第…節 (short).
    if t.starts_with('第')
        && n <= 12
        && (t.contains('章') || t.contains('部') || t.contains('話') || t.contains('節'))
    {
        return true;
    }
    // Chapter N.
    let low = t.to_ascii_lowercase();
    if n <= 20 && (low.starts_with("chapter ") || low.starts_with("chapter\u{a0}")) {
        return true;
    }
    false
}

/// Recursively gather every `link_to` anchor name in a value tree.
fn collect_link_to(value: &IonValue, book: &BookData, out: &mut Vec<String>) {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            if let Some(lt) = get_field(fields, KfxSymbol::LinkTo as u64)
                && let Some(name) = book.symbols.text_of(lt)
            {
                out.push(name.to_string());
            }
            for (_, v) in fields {
                collect_link_to(v, book, out);
            }
        }
        IonValue::List(items) => {
            for it in items {
                collect_link_to(it, book, out);
            }
        }
        _ => {}
    }
}

/// Every `$155 id` in a value tree — used to locate which storyline a landmark
/// eid falls in.
fn collect_ids(value: &IonValue, out: &mut HashSet<i64>) {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            if let Some(id) = get_field(fields, KfxSymbol::Id as u64).and_then(|v| v.as_int()) {
                out.insert(id);
            }
            for (_, v) in fields {
                collect_ids(v, out);
            }
        }
        IonValue::List(items) => {
            for it in items {
                collect_ids(it, out);
            }
        }
        _ => {}
    }
}

/// The eid the `toc`-type landmark targets, if the book has one.
fn toc_landmark_eid(book: &BookData) -> Option<i64> {
    let nav = book.by_type.get(&(KfxSymbol::BookNavigation as u64))?;
    for value in nav.values() {
        let unwrapped = value.unwrap_annotated();
        let candidates: Vec<IonValue> = match unwrapped {
            IonValue::List(items) => items.clone(),
            IonValue::Struct(_) => vec![unwrapped.clone()],
            _ => Vec::new(),
        };
        for reading_order in candidates {
            let Some(ro) = reading_order.as_struct() else {
                continue;
            };
            let Some(containers) =
                get_field(ro, KfxSymbol::NavContainers as u64).and_then(|v| v.as_list())
            else {
                continue;
            };
            for container in containers {
                let Some(resolved) = super::navigation::resolve_nav_container(book, container)
                else {
                    continue;
                };
                let Some(cf) = resolved.as_struct() else {
                    continue;
                };
                let nav_type = get_field(cf, KfxSymbol::NavType as u64)
                    .and_then(|v| book.symbols.text_of(v))
                    .unwrap_or("");
                if nav_type != "landmarks" {
                    continue;
                }
                let Some(entries) =
                    get_field(cf, KfxSymbol::Entries as u64).and_then(|v| v.as_list())
                else {
                    continue;
                };
                for entry in entries {
                    if let Some(eid) = landmark_toc_target(entry) {
                        return Some(eid);
                    }
                }
            }
        }
    }
    None
}

/// If this landmark entry is the `toc` type, its target eid.
fn landmark_toc_target(entry: &IonValue) -> Option<i64> {
    let fields = entry.unwrap_annotated().as_struct()?;
    let lt = get_field(fields, KfxSymbol::LandmarkType as u64)?.as_symbol()?;
    if lt != KfxSymbol::Toc as u64 {
        return None;
    }
    get_field(fields, KfxSymbol::TargetPosition as u64)
        .and_then(|v| v.as_struct())
        .and_then(|pos| get_field(pos, KfxSymbol::Id as u64)?.as_int())
}

/// Front-matter / boilerplate TOC labels (JP + EN). A nav TOC made only of these
/// has no chapters. Kept intentionally broad — the cost of a false "front
/// matter" is only that a nav entry doesn't count toward `nav_chapters`, and the
/// flag still requires strong positive in-book evidence.
fn is_front_matter(label: &str) -> bool {
    let l = label.trim();
    const JP: &[&str] = &[
        "表紙",
        "奥付",
        "目次",
        "もくじ",
        "カバー",
        "扉",
        "中扉",
        "口絵",
        "表題",
        "本扉",
        "標題",
        "凡例",
        "序文",
        "序",
    ];
    for p in JP {
        if l.starts_with(p) {
            return true;
        }
    }
    let low = l.to_ascii_lowercase();
    const EN: &[&str] = &[
        "cover",
        "contents",
        "table of contents",
        "title",
        "copyright",
        "colophon",
        "dedication",
        "about the author",
        "about the publisher",
        "praise",
        "also by",
        "other books",
        "acknowledg",
        "index",
        "front matter",
        "back matter",
        "half title",
    ];
    EN.iter().any(|p| low.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_matches_expected() {
        for s in [
            "表紙",
            "目次",
            "奥付",
            "Cover",
            "Contents",
            "Copyright",
            "About the Author",
        ] {
            assert!(is_front_matter(s), "{s} should be front matter");
        }
        for s in [
            "第一章",
            "Chapter 1",
            "一　目撃者",
            "プロローグ",
            "The High Window",
        ] {
            assert!(!is_front_matter(s), "{s} should not be front matter");
        }
    }

    #[test]
    fn chapter_marker_matches_bare_numbers_and_headings() {
        for s in [
            "1",
            "12",
            "１０",
            "２０",
            "第一章",
            "第三部",
            "第2話",
            "Chapter 5",
        ] {
            assert!(is_chapter_marker(s), "{s} should be a chapter marker");
        }
        for s in [
            "T",
            "SCRIBNER",
            "He was an old man who fished alone",
            "12345",
            "登場人物",
            "はじめに",
        ] {
            assert!(!is_chapter_marker(s), "{s} should not be a chapter marker");
        }
    }
}
