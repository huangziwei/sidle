//! Surgical TOC repair for an EPUB: derive a chapter list from the book's own
//! in-book Contents page (or its headings) and write it into the EPUB 3 nav doc
//! **and** the EPUB 2 NCX, in place — synthesizing either when the book has none.
//!
//! The EPUB analog of [`crate::formats::kfx::toc_repair`]. Where the KFX side rewrites
//! `nav_container` Ion, this edits the two XML documents a reader consults: it
//! **splices** a fresh `<nav epub:type="toc">` into an existing nav doc (leaving
//! its landmarks / page-list untouched) or **synthesizes** a nav doc, and the
//! same for the NCX `<navMap>`; a synthesized doc is registered in the OPF
//! manifest (and the NCX in the spine `toc`). Because EPUB hrefs *are* the nav
//! targets, no eid resolution is needed — the proposer pairs each Contents-page
//! link's text with its href directly.
//!
//! [`propose_toc`] reads the chapter list; [`set_toc`] writes a caller-supplied
//! one; [`repair_toc`] composes them. Public hrefs are **absolute zip paths**
//! (e.g. `"OEBPS/c1.xhtml#ch1"`); [`set_toc`] rebases each to the nav/NCX
//! document it writes. The importer re-derives the KFX nav from whichever of the
//! two is richer, so one repair fixes both the EPUB's own nav and the KFX
//! derived from it.

use std::collections::{HashMap, HashSet};
use std::io;

use crate::formats::epub::edit::{EpubPackage, attr_value, escape_attr};
use crate::formats::epub::nav_doc::{
    depth, render_nav_doc, render_navmap, render_ncx, render_toc_nav,
};
use crate::formats::epub::structure::{
    MIN_SECTION_CONTENTS_LINKS, basename, dir_of, internal_links, relativize, resolve_href,
    spine_documents, split_fragment, strip_fragment,
};
use crate::formats::epub::{OpfData, parse_nav_landmarks, parse_opf, parse_opf_guide};
use crate::model::toc_shape::{TocTree, merge_by_document_order, nest_by_label_indent};
use crate::model::{LandmarkType, TocEntry};
use crate::util::{decode_text, extract_xml_encoding, percent_decode};

/// Minimum distinct chapter links for a page to count as a real Contents page (or
/// headings for the fallback). Below this, a stray cross-reference or two is just
/// noise. A shade lower than the validator's evidence gate because repair is
/// opt-in — the user asked for a TOC and reviews the proposal.
const MIN_CHAPTER_LINKS: usize = 3;

// `MIN_SECTION_CONTENTS_LINKS` is the threshold for the Contents page *inside*
// an already-evidenced volume span (`volume_groups`) — lower than
// `MIN_CHAPTER_LINKS`, which has to tell a Contents page from noise across a
// whole book. It is shared with the splitter, which asks the same question of
// the same spans.

// ---------------------------------------------------------------------------
// Proposer
// ---------------------------------------------------------------------------

/// Derive a chapter list from the EPUB's own structure, for [`set_toc`]. Uses the
/// richest of: the book's own declared TOC (NCX / nav doc, when it lists real
/// chapters) and the densest in-body Contents-page link cluster; falls back to
/// the spine's text headings. Each entry's `href` is an absolute zip path. Empty
/// when nothing usable is found.
pub fn propose_toc(epub_bytes: &[u8]) -> io::Result<Vec<TocEntry>> {
    let pkg = EpubPackage::parse(epub_bytes)?;
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let opf_str = decode_text(pkg.opf_bytes()?, extract_xml_encoding(pkg.opf_bytes()?));
    let opf = parse_opf(&opf_str).map_err(io::Error::other)?;
    Ok(propose_from_pkg(&pkg, &opf, &opf_base, &opf_str))
}

/// How much structure a book's declared TOC has lost — see
/// [`declared_toc_flattening`]. All zeros means nothing to restore.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flattening {
    /// Volumes the book evidences about itself but the TOC lists at the same
    /// depth as their own chapters.
    pub volumes: usize,
    /// Declared top-level entries that belong under one of those volumes.
    pub misplaced: usize,
}

/// Measure what [`repair_toc`] would re-parent: a multi-work book whose
/// declared TOC lists every volume and every chapter at one depth.
///
/// The measurement is the repair's own rule, so a diagnosis and the fix can
/// never disagree. A book that declares its structure, or that has no volume
/// structure to declare, measures zero — the common case, and the early exit.
pub fn declared_toc_flattening(epub_bytes: &[u8]) -> io::Result<Flattening> {
    let pkg = EpubPackage::parse(epub_bytes)?;
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let opf_str = decode_text(pkg.opf_bytes()?, extract_xml_encoding(pkg.opf_bytes()?));
    let opf = parse_opf(&opf_str).map_err(io::Error::other)?;

    let declared = existing_declared_toc(&pkg, &opf, &opf_base);
    if declared.len() < 2 || declared.iter().any(|e| !e.children.is_empty()) {
        return Ok(Flattening::default());
    }
    let declared_top = declared.len();
    let targets: HashSet<&str> = declared.iter().map(|e| strip_fragment(&e.href)).collect();
    let groups = volume_groups(&pkg, &opf, &opf_base, &targets);
    let nested = nest_by_volume_groups(declared.clone(), &groups);
    Ok(Flattening {
        volumes: nested.iter().filter(|e| !e.children.is_empty()).count(),
        misplaced: declared_top - nested.len(),
    })
}

/// The proposal for an already-parsed package — see [`propose_toc`], which is
/// this plus the parsing. Split detection reads the same proposal without
/// paying for a second pass over the zip.
pub(super) fn propose_from_pkg(
    pkg: &EpubPackage,
    opf: &OpfData,
    opf_base: &str,
    opf_str: &str,
) -> Vec<TocEntry> {
    // 1. The book's own authored TOC (NCX / nav doc). Whatever else is found, it
    //    is never discarded: it is the only source that knows the entries no
    //    Contents page links (the cover, the Contents page itself, the colophon),
    //    and dropping one would make a "repair" a regression for the reader.
    let declared = existing_declared_toc(pkg, opf, opf_base);

    // 2. The densest in-body Contents-page link cluster, else — only when
    //    neither it nor the declared TOC knows any chapters — one entry per spine
    //    doc that opens with a text heading. Image-heavy books render both the
    //    Contents page and the chapter headings as images, so there is nothing to
    //    derive from and the declared TOC is all there is.
    let spine = spine_documents(opf, opf_base);
    let contents = contents_page_links(pkg, opf, opf_base, opf_str);
    let derived = if contents.len() >= MIN_CHAPTER_LINKS {
        contents
    } else if count_chapters(&declared) >= MIN_CHAPTER_LINKS {
        Vec::new()
    } else {
        let headings = propose_from_headings(pkg, &spine);
        if headings.len() >= MIN_CHAPTER_LINKS {
            headings
        } else {
            Vec::new()
        }
    };

    // 3. Merge, in spine order: every declared entry survives, and a derived one
    //    joins it wherever the declared TOC doesn't already reach. The cover and
    //    the Contents page itself are deliberately not added from the book's
    //    landmarks — a reader reaches those because the renderer composes the
    //    landmarks into its own view, and writing them in here would list them
    //    twice for every book whose publisher already declared them.
    let positions: HashMap<&str, usize> = spine
        .iter()
        .enumerate()
        .map(|(i, (abs, _))| (abs.as_str(), i))
        .collect();
    let entries = merge_by_document_order(declared, derived, |e| {
        positions.get(strip_fragment(&e.href)).copied()
    });

    // 4. Restore the levels a flattened TOC lost — to whatever depth the book
    //    evidences. A no-op unless it evidences any.
    let targets: HashSet<&str> = entries.iter().map(|e| strip_fragment(&e.href)).collect();
    let groups = volume_groups(pkg, opf, opf_base, &targets);
    nest_levels(entries, &groups)
}

/// Restore every level a flattened TOC lost, to the depth the book itself
/// evidences — **not** to a fixed number of levels.
///
/// Two signals compose, and each is re-applied inside whatever the other
/// produced, so a 部 that contains 巻 that contain 章 that contain 節 comes out
/// four deep without anything here counting levels:
///
/// 1. **Volume grouping** ([`nest_by_volume_groups`]) — a section that opens
///    with its own cover page and names its contents. Nests to any depth by
///    containment: a start a enclosing volume's Contents page lists is that
///    volume's child, not its sibling.
/// 2. **Label indentation** ([`nest_by_label_indent`]) — the levels a publisher
///    kept as leading whitespace when the NCX lost them. Arbitrary depth by
///    construction.
fn nest_levels(entries: Vec<TocEntry>, groups: &[VolumeGroup]) -> Vec<TocEntry> {
    let mut out = nest_by_label_indent(nest_by_volume_groups(entries, groups));
    for entry in &mut out {
        if !entry.children.is_empty() {
            entry.children = nest_levels(std::mem::take(&mut entry.children), groups);
        }
    }
    out
}

/// One volume/part of a multi-work book, as the book itself evidences it: a
/// full-bleed image page (the volume's own cover) opens it, and a Contents page
/// inside its span enumerates its chapters.
struct VolumeGroup {
    /// Absolute zip path of the volume's opening page.
    start: String,
    /// Absolute zip paths belonging to this volume — its Contents page and
    /// everything that page links to.
    members: HashSet<String>,
}

/// The volume groupings a book declares about itself, in spine order.
///
/// Deliberately narrow, because the signals are individually weak. A volume
/// start must be all three of: a page of pictures, a page the TOC actually
/// points at (`targets` — an illustration plate is an image page too, and a
/// light novel carries hundreds), and not the first spine document (the book's
/// own cover starts no volume). A start then only forms a group if a Contents
/// page within its span names its chapters, and fewer than two groups is no
/// collection — one volume never was one, and a book that forms exactly one has
/// found its own front matter rather than a volume. So a book with no volume
/// structure is never invented one, and back matter that no volume's Contents
/// page lists is never swallowed by the volume in front of it.
fn volume_groups(
    pkg: &EpubPackage,
    opf: &OpfData,
    opf_base: &str,
    targets: &HashSet<&str>,
) -> Vec<VolumeGroup> {
    let spine = spine_documents(opf, opf_base);
    let spine_files: HashSet<&str> = spine.iter().map(|(_, b)| b.as_str()).collect();

    let read = |abs: &str| -> Option<String> {
        let bytes = pkg.get(abs)?;
        Some(decode_text(bytes, extract_xml_encoding(bytes)).into_owned())
    };

    let starts: Vec<usize> = spine
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, (abs, _))| {
            targets.contains(abs.as_str())
                && read(abs).is_some_and(|x| super::page_shape::is_image_only_page(&x))
        })
        .map(|(i, _)| i)
        .collect();
    if starts.len() < 2 {
        return Vec::new();
    }

    let mut groups = Vec::new();
    for (n, &start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(spine.len());
        for (abs, base) in &spine[start..end] {
            let Some(xhtml) = read(abs) else { continue };
            let links = dedup_entries(internal_links(&xhtml, &dir_of(abs), &spine_files, base));
            if links.len() < MIN_SECTION_CONTENTS_LINKS {
                continue;
            }
            // The volume's own Contents page — the first one in its span.
            let mut members: HashSet<String> = links
                .into_iter()
                .map(|e| strip_fragment(&e.href).to_string())
                .collect();
            members.insert(abs.clone());
            groups.push(VolumeGroup {
                start: spine[start].0.clone(),
                members,
            });
            break;
        }
    }
    if groups.len() < 2 { Vec::new() } else { groups }
}

/// Re-parent a flat entry list into the volumes [`volume_groups`] found: an
/// entry that opens a volume becomes a parent, and the entries after it that
/// its own Contents page names become its children.
///
/// **Volumes nest inside volumes** — the open volumes form a stack, and a
/// volume start that an enclosing volume's Contents page lists becomes that
/// volume's child rather than its sibling. Nothing here fixes a depth: a book
/// that stacks 部 → 巻 → 篇 comes out that deep.
///
/// A TOC that already declares nesting is left alone, an entry no open volume
/// claims closes them until one does (or lands at the top level), and a book
/// whose groups adopt nothing comes back unchanged — this pass only ever
/// restores structure the book states about itself.
fn nest_by_volume_groups(entries: Vec<TocEntry>, groups: &[VolumeGroup]) -> Vec<TocEntry> {
    if groups.is_empty() || entries.iter().any(|e| !e.children.is_empty()) {
        return entries;
    }
    let mut tree = TocTree::with_capacity(entries.len());
    // The volumes currently open, outermost first: `(node, its group)`.
    let mut open: Vec<(usize, &VolumeGroup)> = Vec::new();
    for entry in entries {
        let doc = strip_fragment(&entry.href).to_string();
        // Close every open volume that does not claim this entry.
        while open.last().is_some_and(|(_, g)| !g.members.contains(&doc)) {
            open.pop();
        }
        let node = tree.push(entry, open.last().map(|&(parent, _)| parent));
        if let Some(group) = groups.iter().find(|g| g.start == doc) {
            open.push((node, group));
        }
    }
    tree.build()
}

/// The densest cluster of internal chapter links across the spine — a Contents
/// page. Prefers the page a `toc` landmark marks (guards against a link-dense
/// chapter out-linking the real Contents page). Empty if none carries links.
fn contents_page_links(
    pkg: &EpubPackage,
    opf: &OpfData,
    opf_base: &str,
    opf_str: &str,
) -> Vec<TocEntry> {
    let spine = spine_documents(opf, opf_base);
    let spine_files: HashSet<&str> = spine.iter().map(|(_, b)| b.as_str()).collect();
    let landmark_file = toc_landmark_basename(pkg, opf, opf_base, opf_str);

    let mut best: Vec<TocEntry> = Vec::new();
    let mut landmark_hit: Option<Vec<TocEntry>> = None;
    for (abs, base) in &spine {
        let Some(bytes) = pkg.get(abs) else { continue };
        let xhtml = decode_text(bytes, extract_xml_encoding(bytes));
        let doc_dir = dir_of(abs);
        let links = internal_links(&xhtml, &doc_dir, &spine_files, base);
        let entries = dedup_entries(links);
        if entries.len() > best.len() {
            best = entries.clone();
        }
        if landmark_hit.is_none()
            && landmark_file.as_deref() == Some(base.as_str())
            && entries.len() >= MIN_CHAPTER_LINKS
        {
            landmark_hit = Some(entries);
        }
    }
    landmark_hit.unwrap_or(best)
}

/// The book's declared TOC as each of the two documents a reader may consult has
/// it — `(NCX, EPUB-3 nav doc)` — with every href resolved to an absolute zip
/// path so it can be re-emitted by [`set_toc`]. Either may be empty; they can
/// also disagree, which is itself something [`repair_toc`] fixes.
fn declared_toc_documents(
    pkg: &EpubPackage,
    opf: &OpfData,
    opf_base: &str,
) -> (Vec<TocEntry>, Vec<TocEntry>) {
    let read = |href: &str, parse: fn(&str) -> io::Result<Vec<TocEntry>>| -> Vec<TocEntry> {
        let abs = format!("{opf_base}{}", percent_decode(href));
        let Some(bytes) = pkg.get(&abs) else {
            return Vec::new();
        };
        let text = decode_text(bytes, extract_xml_encoding(bytes));
        let entries = parse(&text).unwrap_or_default();
        // NCX/nav hrefs are relative to that document's own directory.
        rebase_to_absolute(&entries, &dir_of(&abs))
    };
    let ncx = opf
        .ncx_href
        .as_deref()
        .map(|h| read(h, crate::formats::epub::parse_ncx))
        .unwrap_or_default();
    let nav = opf
        .nav_href
        .as_deref()
        .map(|h| read(h, crate::formats::epub::parse_nav_toc))
        .unwrap_or_default();
    (ncx, nav)
}

/// The book's declared TOC: the richer of its two nav documents.
pub(super) fn existing_declared_toc(
    pkg: &EpubPackage,
    opf: &OpfData,
    opf_base: &str,
) -> Vec<TocEntry> {
    let (ncx, nav) = declared_toc_documents(pkg, opf, opf_base);
    if flat_count(&ncx) >= flat_count(&nav) {
        ncx
    } else {
        nav
    }
}

/// Resolve every entry's (doc-relative) href to an absolute zip path, recursively.
fn rebase_to_absolute(entries: &[TocEntry], base_dir: &str) -> Vec<TocEntry> {
    entries
        .iter()
        .map(|e| {
            let href = if e.href.starts_with('#') || e.href.is_empty() {
                e.href.clone()
            } else {
                resolve_href(base_dir, &e.href)
            };
            let mut t = TocEntry::new(e.title.clone(), href);
            t.children = rebase_to_absolute(&e.children, base_dir);
            t
        })
        .collect()
}

/// Count entries whose label is a real chapter (not front-matter boilerplate),
/// recursively — the gate for reusing an existing declared TOC.
fn count_chapters(entries: &[TocEntry]) -> usize {
    entries
        .iter()
        .map(|e| {
            let this = usize::from(!crate::validate::source::toc::is_front_matter(&e.title));
            this + count_chapters(&e.children)
        })
        .sum()
}

/// Total entries in a tree, counting nested children.
fn flat_count(entries: &[TocEntry]) -> usize {
    entries.iter().map(|e| 1 + flat_count(&e.children)).sum()
}

/// One entry per spine doc whose first heading (`<h1>`..`<h6>`) carries text: the
/// heading text as label, the doc (plus the heading's `id`, if any) as target.
fn propose_from_headings(pkg: &EpubPackage, spine: &[(String, String)]) -> Vec<TocEntry> {
    let mut out = Vec::new();
    for (abs, _) in spine {
        let Some(bytes) = pkg.get(abs) else { continue };
        let xhtml = decode_text(bytes, extract_xml_encoding(bytes));
        if let Some((label, id)) = first_heading(&xhtml) {
            let href = match id {
                Some(id) => format!("{abs}#{id}"),
                None => abs.clone(),
            };
            out.push(TocEntry::new(label, href));
        }
    }
    out
}

/// Collapse `(label, href)` links to one [`TocEntry`] per distinct target,
/// keeping document order and the first non-empty label seen. A link whose label
/// is blank still anchors an entry (labeled by its filename as a last resort).
fn dedup_entries(links: Vec<(String, String)>) -> Vec<TocEntry> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for (label, href) in links {
        if !seen.insert(href.clone()) {
            continue;
        }
        let label = clean_label(&label);
        let label = if label.is_empty() {
            basename(&href)
        } else {
            label
        };
        out.push(TocEntry::new(label, href));
    }
    out
}

/// Every landmark the book declares, hrefs resolved to absolute zip paths: the
/// EPUB 3 nav doc's `landmarks` first, then the EPUB 2 `<guide>` — which older
/// readers still consult, and which is all an EPUB 2 has. A book carrying both
/// usually says the same thing twice; callers keep the first of each target.
fn book_landmarks(
    pkg: &EpubPackage,
    opf: &OpfData,
    opf_base: &str,
    opf_str: &str,
) -> Vec<crate::model::Landmark> {
    let rebase = |lms: Vec<crate::model::Landmark>, dir: &str| -> Vec<crate::model::Landmark> {
        lms.into_iter()
            .map(|l| crate::model::Landmark {
                href: resolve_href(dir, &l.href),
                ..l
            })
            .collect()
    };
    let mut out = Vec::new();
    if let Some(nav_href) = &opf.nav_href {
        let abs = format!("{opf_base}{}", percent_decode(nav_href));
        if let Some(bytes) = pkg.get(&abs) {
            let nav = decode_text(bytes, extract_xml_encoding(bytes));
            out.extend(rebase(
                parse_nav_landmarks(&nav).unwrap_or_default(),
                &dir_of(&abs),
            ));
        }
    }
    out.extend(rebase(
        parse_opf_guide(opf_str).unwrap_or_default(),
        opf_base,
    ));
    out
}

/// The spine-doc basename a `toc`-type landmark points at.
fn toc_landmark_basename(
    pkg: &EpubPackage,
    opf: &OpfData,
    opf_base: &str,
    opf_str: &str,
) -> Option<String> {
    book_landmarks(pkg, opf, opf_base, opf_str)
        .iter()
        .find(|l| l.landmark_type == LandmarkType::Toc)
        .map(|l| basename(&l.href))
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Drop every `#fragment` its target document doesn't actually define.
///
/// A book's own Contents page can link anchors that a conversion lost. A
/// proposal mined from that page would otherwise write the dead fragments into
/// the nav doc *and* the NCX, turning one broken page into three. The entry
/// itself survives — the document is still the right place to land — it just
/// points at the document instead of at an anchor that isn't there.
///
/// A fragment whose document isn't in the container is left alone: the entry is
/// broken either way, and that is a different defect with its own report.
fn resolve_fragments(pkg: &EpubPackage, entries: &[TocEntry]) -> Vec<TocEntry> {
    let mut wanted: HashMap<&str, HashSet<&str>> = HashMap::new();
    collect_fragments(entries, &mut wanted);
    if wanted.is_empty() {
        return entries.to_vec();
    }
    // One read per document, however many entries land in it.
    let mut defined: HashSet<(&str, &str)> = HashSet::new();
    for (doc, fragments) in &wanted {
        let Some(bytes) = pkg.get(&percent_decode(doc)) else {
            defined.extend(fragments.iter().map(|f| (*doc, *f)));
            continue;
        };
        let xhtml = decode_text(bytes, extract_xml_encoding(bytes));
        for fragment in fragments {
            if defines_fragment(&xhtml, &percent_decode(fragment)) {
                defined.insert((doc, fragment));
            }
        }
    }
    strip_undefined_fragments(entries, &defined)
}

/// The fragment an href names, without its `#` — the form that has to match an
/// `id` attribute. Empty when the href carries none.
fn fragment_id(href: &str) -> &str {
    split_fragment(href).1.trim_start_matches('#')
}

fn collect_fragments<'a>(entries: &'a [TocEntry], out: &mut HashMap<&'a str, HashSet<&'a str>>) {
    for entry in entries {
        let doc = split_fragment(&entry.href).0;
        let id = fragment_id(&entry.href);
        if !id.is_empty() && !doc.is_empty() {
            out.entry(doc).or_default().insert(id);
        }
        collect_fragments(&entry.children, out);
    }
}

fn strip_undefined_fragments(
    entries: &[TocEntry],
    defined: &HashSet<(&str, &str)>,
) -> Vec<TocEntry> {
    entries
        .iter()
        .map(|entry| {
            let doc = split_fragment(&entry.href).0;
            let id = fragment_id(&entry.href);
            let href = if id.is_empty() || defined.contains(&(doc, id)) {
                entry.href.clone()
            } else {
                doc.to_string()
            };
            let mut out = TocEntry::new(entry.title.clone(), href);
            out.children = strip_undefined_fragments(&entry.children, defined);
            out
        })
        .collect()
}

/// True if `xhtml` defines `id` as a fragment target: an `id` attribute with
/// that value, or the older `<a name>` form.
///
/// Deliberately permissive — a scan, not a parse. Guessing "defined" leaves a
/// working target alone, while guessing "not defined" would demote a good anchor
/// to its document, so anything that reads like the attribute counts.
fn defines_fragment(xhtml: &str, id: &str) -> bool {
    for attr in ["id=", "name="] {
        let mut rest: &str = xhtml;
        while let Some(at) = rest.find(attr) {
            rest = &rest[at + attr.len()..];
            let quote = rest.chars().next();
            let value = match quote {
                Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or(""),
                _ => continue,
            };
            if value == id {
                return true;
            }
        }
    }
    false
}

/// Write `entries` into the EPUB's nav doc and NCX, in place — splicing over an
/// existing toc / navMap, or synthesizing (and registering in the OPF) when the
/// book has none. `entries` hrefs are absolute zip paths, and any `#fragment`
/// the target document doesn't define is dropped (`resolve_fragments`).
///
/// Errors if `entries` is empty or the bytes aren't a readable EPUB.
pub fn set_toc(epub_bytes: &[u8], entries: &[TocEntry]) -> io::Result<Vec<u8>> {
    if entries.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to write an empty TOC",
        ));
    }
    let mut pkg = EpubPackage::parse(epub_bytes)?;
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let opf_raw =
        decode_text(pkg.opf_bytes()?, extract_xml_encoding(pkg.opf_bytes()?)).into_owned();
    let opf = parse_opf(&opf_raw).map_err(io::Error::other)?;

    let entries = &resolve_fragments(&pkg, entries);

    let title = if opf.metadata.title.is_empty() {
        "Contents"
    } else {
        &opf.metadata.title
    };
    let lang = if opf.metadata.language.is_empty() {
        "en"
    } else {
        &opf.metadata.language
    };
    // An NCX's `dtb:uid` must be the identifier the OPF designates as unique —
    // not merely one of the book's identifiers, which is a different question
    // when a book carries an ASIN and a calibre id beside its UUID.
    let uid = opf
        .unique_identifier
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("urn:uuid:bokai-repaired-toc");

    let mut opf_str = opf_raw;
    let mut opf_dirty = false;

    // Whichever nav document the book already has is rewritten. A *missing* one
    // is only synthesized when the package's own version asks for it: an EPUB 3
    // must carry a nav doc, an EPUB 2 must carry an NCX, and adding the other is
    // a version change nobody requested — one that, in an EPUB 2 package, an
    // `properties="nav"` manifest item doesn't even validate as.
    let epub2 = opf.version.starts_with('2');

    // --- nav doc (EPUB 3) ---
    let nav_abs = opf
        .nav_href
        .as_deref()
        .map(|h| format!("{opf_base}{}", percent_decode(h)))
        .unwrap_or_else(|| format!("{opf_base}nav.xhtml"));
    let nav_dir = dir_of(&nav_abs);
    let toc_nav = render_toc_nav(entries, &nav_dir);
    match pkg.get(&nav_abs) {
        Some(bytes) => {
            let spliced =
                splice_toc_nav(&decode_text(bytes, extract_xml_encoding(bytes)), &toc_nav);
            pkg.set(&nav_abs, spliced.into_bytes());
        }
        None if !epub2 => {
            // Synthesize and register in the OPF manifest.
            let id = free_id(&opf, "nav");
            let rel = relativize(&opf_base, &nav_abs);
            opf_str = add_manifest_item(&opf_str, &id, &rel, "application/xhtml+xml", Some("nav"));
            opf_dirty = true;
            pkg.set(&nav_abs, render_nav_doc(&toc_nav, lang, title).into_bytes());
        }
        None => {}
    }

    // --- NCX (EPUB 2) ---
    let ncx_abs = opf
        .ncx_href
        .as_deref()
        .map(|h| format!("{opf_base}{}", percent_decode(h)))
        .unwrap_or_else(|| format!("{opf_base}toc.ncx"));
    let ncx_dir = dir_of(&ncx_abs);
    let navmap = render_navmap(entries, &ncx_dir);
    match pkg.get(&ncx_abs) {
        Some(bytes) => {
            let spliced = splice_navmap(&decode_text(bytes, extract_xml_encoding(bytes)), &navmap);
            pkg.set(&ncx_abs, spliced.into_bytes());
        }
        None if epub2 => {
            let id = free_id(&opf, "ncx");
            let rel = relativize(&opf_base, &ncx_abs);
            opf_str = add_manifest_item(&opf_str, &id, &rel, "application/x-dtbncx+xml", None);
            opf_str = ensure_spine_toc(&opf_str, &id);
            opf_dirty = true;
            pkg.set(
                &ncx_abs,
                render_ncx(&navmap, uid, title, depth(entries)).into_bytes(),
            );
        }
        None => {}
    }

    if opf_dirty {
        pkg.replace(&opf_path, opf_str.into_bytes());
    }
    pkg.into_bytes()
}

/// One-call repair: [`propose_toc`] then [`set_toc`]. Errors if no chapter list
/// can be derived, or the bytes aren't a readable EPUB.
pub fn repair_toc(epub_bytes: &[u8]) -> io::Result<Vec<u8>> {
    let pkg = EpubPackage::parse(epub_bytes)?;
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let opf_str = decode_text(pkg.opf_bytes()?, extract_xml_encoding(pkg.opf_bytes()?));
    let opf = parse_opf(&opf_str).map_err(io::Error::other)?;

    let entries = propose_from_pkg(&pkg, &opf, &opf_base, &opf_str);
    if entries.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no declared TOC and no in-book chapter list to rebuild one from",
        ));
    }
    // Since the proposal starts from the declared TOC, a book with nothing to
    // add and no structure to restore proposes exactly what it already has.
    // Writing that back is not a repair — say so, rather than report a fix that
    // changed nothing.
    //
    // What has to already be right is the EPUB 3 nav doc, plus the NCX *if the
    // book has one*: a missing or disagreeing nav doc is a real defect this
    // write fixes, while a missing NCX is not — EPUB 3 does not ask for one, and
    // synthesizing it would be a change nobody requested.
    let (ncx, nav) = declared_toc_documents(&pkg, &opf, &opf_base);
    if entries == nav && (ncx.is_empty() || entries == ncx) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the declared TOC already lists everything the book evidences",
        ));
    }
    set_toc(epub_bytes, &entries)
}

// ---------------------------------------------------------------------------
// XML splicing (preserve everything but the one element we rewrite)
// ---------------------------------------------------------------------------

/// Replace the existing `<nav epub:type="toc">…</nav>` with `new_nav`, or insert
/// `new_nav` before `</body>` if the doc has no toc nav yet.
fn splice_toc_nav(nav_xhtml: &str, new_nav: &str) -> String {
    if let Some((start, end)) = find_toc_nav_span(nav_xhtml) {
        format!("{}{new_nav}{}", &nav_xhtml[..start], &nav_xhtml[end..])
    } else {
        insert_before(nav_xhtml, "</body>", &format!("{new_nav}\n"))
    }
}

/// Replace the existing `<navMap>…</navMap>` with `new_navmap`, or insert it
/// before `</ncx>` if the NCX has none.
fn splice_navmap(ncx_xml: &str, new_navmap: &str) -> String {
    if let Some((start, end)) = find_element_span(ncx_xml, "navMap") {
        format!("{}{new_navmap}{}", &ncx_xml[..start], &ncx_xml[end..])
    } else {
        insert_before(ncx_xml, "</ncx>", &format!("{new_navmap}\n"))
    }
}

/// Byte range of the whole `<nav …epub:type/​type contains toc…>…</nav>`.
fn find_toc_nav_span(xhtml: &str) -> Option<(usize, usize)> {
    let bytes = xhtml.as_bytes();
    let mut from = 0;
    while let Some(rel) = xhtml[from..].find("<nav") {
        let tag_start = from + rel;
        let after = bytes.get(tag_start + 4).copied().unwrap_or(b' ');
        if !(after == b'>' || after.is_ascii_whitespace()) {
            from = tag_start + 4;
            continue;
        }
        let Some(gt) = xhtml[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + gt + 1;
        if nav_tag_is_toc(&xhtml[tag_start..tag_end]) {
            let close = xhtml[tag_end..].find("</nav>")?;
            return Some((tag_start, tag_end + close + "</nav>".len()));
        }
        from = tag_end;
    }
    None
}

/// Byte range of the first `<name …>…</name>` element (no nesting assumed).
fn find_element_span(xml: &str, name: &str) -> Option<(usize, usize)> {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let start = xml.find(&open)?;
    // Confirm a real tag boundary (`<navMap>` or `<navMap …>`), not a prefix.
    let after = xml
        .as_bytes()
        .get(start + open.len())
        .copied()
        .unwrap_or(b' ');
    if !(after == b'>' || after.is_ascii_whitespace()) {
        return None;
    }
    let end_rel = xml[start..].find(&close)?;
    Some((start, start + end_rel + close.len()))
}

fn nav_tag_is_toc(start_tag: &str) -> bool {
    ["epub:type", "type"].iter().any(|key| {
        attr_value(start_tag, key).is_some_and(|v| v.split_whitespace().any(|t| t == "toc"))
    })
}

/// Insert `insertion` immediately before the last occurrence of `anchor`
/// (case-sensitive); append it if the anchor is absent.
fn insert_before(haystack: &str, anchor: &str, insertion: &str) -> String {
    match haystack.rfind(anchor) {
        Some(pos) => format!("{}{insertion}{}", &haystack[..pos], &haystack[pos..]),
        None => format!("{haystack}{insertion}"),
    }
}

// ---------------------------------------------------------------------------
// OPF editing (register a synthesized nav doc / NCX)
// ---------------------------------------------------------------------------

/// A manifest id not already in use, from `preferred` (`preferred`, then
/// `preferred-2`, …).
fn free_id(opf: &OpfData, preferred: &str) -> String {
    if !opf.manifest.contains_key(preferred) {
        return preferred.to_string();
    }
    (2..)
        .map(|n| format!("{preferred}-{n}"))
        .find(|id| !opf.manifest.contains_key(id))
        .unwrap_or_else(|| format!("{preferred}-bokai"))
}

/// Insert an `<item>` into the OPF `<manifest>`, before `</manifest>`.
fn add_manifest_item(
    opf: &str,
    id: &str,
    href: &str,
    media_type: &str,
    properties: Option<&str>,
) -> String {
    let props = properties
        .map(|p| format!(" properties=\"{}\"", escape_attr(p)))
        .unwrap_or_default();
    let item = format!(
        "  <item id=\"{}\" href=\"{}\" media-type=\"{}\"{}/>\n",
        escape_attr(id),
        escape_attr(href),
        escape_attr(media_type),
        props
    );
    insert_before(opf, "</manifest>", &item)
}

/// Ensure the OPF `<spine>` carries `toc="{ncx_id}"` (add it if absent; leave any
/// existing `toc` reference alone).
fn ensure_spine_toc(opf: &str, ncx_id: &str) -> String {
    let Some(start) = opf.find("<spine") else {
        return opf.to_string();
    };
    let Some(gt) = opf[start..].find('>') else {
        return opf.to_string();
    };
    let tag_end = start + gt;
    let tag = &opf[start..tag_end];
    if attr_value(tag, "toc").is_some() {
        return opf.to_string(); // already references an NCX
    }
    format!(
        "{}<spine toc=\"{}\"{}",
        &opf[..start],
        escape_attr(ncx_id),
        &opf[start + "<spine".len()..]
    )
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Collapse a raw link text to a nav label: ASCII whitespace runs → one space
/// (full-width spacing in JP titles preserved), then trim.
fn clean_label(raw: &str) -> String {
    let mut s = String::with_capacity(raw.len());
    let mut prev_space = false;
    for c in raw.chars() {
        if matches!(c, ' ' | '\n' | '\r' | '\t') {
            if !prev_space {
                s.push(' ');
                prev_space = true;
            }
        } else {
            s.push(c);
            prev_space = false;
        }
    }
    // Trim the same ASCII set the collapse above uses. `str::trim` would drop a
    // leading or trailing U+3000, contradicting the preservation this function
    // exists to provide.
    crate::util::trim_markup_space(&s).to_string()
}

/// First `<h1>`..`<h6>`'s `(text, id)` — the heading's stripped text and its
/// `id` attribute, if any. `None` when the doc has no heading with text.
fn first_heading(xhtml: &str) -> Option<(String, Option<String>)> {
    let body = xhtml.split_once("<body").map(|(_, b)| b).unwrap_or(xhtml);
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b'<'
            && (bytes[i + 1] == b'h' || bytes[i + 1] == b'H')
            && (b'1'..=b'6').contains(&bytes[i + 2])
        {
            let after = bytes.get(i + 3).copied().unwrap_or(b' ');
            if after == b'>' || after.is_ascii_whitespace() {
                let gt = body[i..].find('>')?;
                let tag = &body[i..i + gt];
                let id = attr_value(tag, "id");
                let close_lower = format!("</h{}>", (bytes[i + 2] as char));
                let content_start = i + gt + 1;
                let close = body[content_start..]
                    .to_ascii_lowercase()
                    .find(&close_lower)?;
                let text = clean_label(&crate::util::strip_tags(
                    &body[content_start..content_start + close],
                ));
                if !text.is_empty() {
                    return Some((text, id));
                }
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Book;
    use crate::model::Format;
    use std::io::Write;

    const FIXTURE: &str = "tests/fixtures/[太宰 治] 人間失格.epub";

    /// Build an EPUB whose Contents page links chapters by `#fragment`, some of
    /// which the chapter documents don't define — what a conversion that dropped
    /// anchors leaves behind. `version` and the identifier list are caller-set so
    /// one builder covers the writer's version and `dtb:uid` rules too.
    fn epub_with_broken_anchors(version: &str, identifiers: &str) -> Vec<u8> {
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
                r#"<item id="toc-page" href="toc.xhtml" media-type="application/xhtml+xml"/>"#,
            );
            let mut spine = String::from(r#"<itemref idref="toc-page"/>"#);
            let mut toc_links = String::new();
            for n in 1..=6 {
                manifest.push_str(&format!(
                    r#"<item id="c{n}" href="c{n}.xhtml" media-type="application/xhtml+xml"/>"#
                ));
                spine.push_str(&format!(r#"<itemref idref="c{n}"/>"#));
                // Odd chapters define the anchor the Contents page names; even
                // ones don't.
                let anchor = if n % 2 == 1 {
                    format!(r#"<h1 id="a{n}">Chapter {n}</h1>"#)
                } else {
                    format!("<h1>Chapter {n}</h1>")
                };
                toc_links.push_str(&format!(
                    r#"<li><a href="c{n}.xhtml#a{n}">Chapter {n}</a></li>"#
                ));
                add(
                    &format!("OEBPS/c{n}.xhtml"),
                    &format!(
                        r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>C{n}</title></head><body>{anchor}<p>body {n}</p></body></html>"#
                    ),
                );
            }
            add(
                "OEBPS/content.opf",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><package xmlns="http://www.idpf.org/2007/opf" version="{version}" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Anchored Book</dc:title><dc:language>en</dc:language>{identifiers}</metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
                ),
            );
            add(
                "OEBPS/toc.xhtml",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>Contents</title></head><body><h1>Contents</h1><ul>{toc_links}</ul></body></html>"#
                ),
            );
            zip.finish().unwrap();
        }
        buf
    }

    /// A TOC entry pointing at an anchor its document doesn't define would be a
    /// broken link in the nav doc *and* in the NCX — one broken Contents page
    /// turned into three. The entry survives, aimed at the document.
    #[test]
    fn a_fragment_the_target_doesnt_define_is_dropped() {
        let epub = epub_with_broken_anchors(
            "3.0",
            r#"<dc:identifier id="uid">urn:uuid:anchored</dc:identifier>"#,
        );
        let out = repair_toc(&epub).expect("repair");
        let pkg = EpubPackage::parse(&out).expect("parse");
        let nav = String::from_utf8(pkg.get("OEBPS/nav.xhtml").expect("nav doc").to_vec()).unwrap();

        assert!(
            nav.contains(r#"href="c1.xhtml#a1""#),
            "kept a real anchor: {nav}"
        );
        assert!(
            nav.contains(r#"href="c2.xhtml""#) && !nav.contains("#a2"),
            "dropped the undefined anchor but kept the entry: {nav}"
        );
        // Every chapter is still listed — dropping a dead fragment must not drop
        // the entry with it.
        for n in 1..=6 {
            assert!(
                nav.contains(&format!("Chapter {n}")),
                "lost Chapter {n}: {nav}"
            );
        }
    }

    /// `dtb:uid` must be the identifier `<package unique-identifier>` names —
    /// not merely the first one, which for a book carrying an ASIN and a calibre
    /// id beside its UUID is the wrong one (epubcheck NCX-001).
    #[test]
    fn the_ncx_uid_is_the_opfs_unique_identifier() {
        let epub = epub_with_broken_anchors(
            "2.0",
            r#"<dc:identifier>mobi-asin:B000000000</dc:identifier><dc:identifier>calibre:99</dc:identifier><dc:identifier id="uid">urn:uuid:the-real-one</dc:identifier>"#,
        );
        let out = repair_toc(&epub).expect("repair");
        let pkg = EpubPackage::parse(&out).expect("parse");
        let ncx = String::from_utf8(pkg.get("OEBPS/toc.ncx").expect("ncx").to_vec()).unwrap();

        assert!(
            ncx.contains(r#"content="urn:uuid:the-real-one""#),
            "dtb:uid must match the OPF unique identifier: {}",
            &ncx[..ncx.len().min(400)]
        );
    }

    /// Repair fixes a book's TOC; it does not change which EPUB version the book
    /// is. An EPUB 2 gets its NCX and no nav document (a `properties="nav"`
    /// manifest item doesn't even validate there); an EPUB 3 gets its nav
    /// document and no NCX, which EPUB 3 never required.
    #[test]
    fn repair_does_not_change_the_books_epub_version() {
        let ids = r#"<dc:identifier id="uid">urn:uuid:versioned</dc:identifier>"#;

        let out2 = repair_toc(&epub_with_broken_anchors("2.0", ids)).expect("repair epub2");
        let pkg2 = EpubPackage::parse(&out2).expect("parse");
        assert!(pkg2.get("OEBPS/toc.ncx").is_some(), "EPUB 2 gets its NCX");
        assert!(
            pkg2.get("OEBPS/nav.xhtml").is_none(),
            "EPUB 2 must not gain an EPUB 3 nav document"
        );

        let out3 = repair_toc(&epub_with_broken_anchors("3.0", ids)).expect("repair epub3");
        let pkg3 = EpubPackage::parse(&out3).expect("parse");
        assert!(
            pkg3.get("OEBPS/nav.xhtml").is_some(),
            "EPUB 3 gets its nav doc"
        );
        assert!(
            pkg3.get("OEBPS/toc.ncx").is_none(),
            "EPUB 3 must not gain an NCX it never required"
        );
    }

    /// Build a no-TOC EPUB: a Contents page links to 6 chapters, but the OPF
    /// declares no nav doc and no NCX. This is the "no toc epub" repair case.
    fn no_toc_epub() -> Vec<u8> {
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
                r#"<item id="toc-page" href="toc.xhtml" media-type="application/xhtml+xml"/>"#,
            );
            let mut spine = String::from(r#"<itemref idref="toc-page"/>"#);
            let mut toc_links = String::new();
            for n in 1..=6 {
                manifest.push_str(&format!(
                    r#"<item id="c{n}" href="c{n}.xhtml" media-type="application/xhtml+xml"/>"#
                ));
                spine.push_str(&format!(r#"<itemref idref="c{n}"/>"#));
                toc_links.push_str(&format!(r#"<li><a href="c{n}.xhtml">Chapter {n}</a></li>"#));
                add(
                    &format!("OEBPS/c{n}.xhtml"),
                    &format!(
                        r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>C{n}</title></head><body><h1 id="h{n}">Chapter {n}</h1><p>body {n}</p></body></html>"#
                    ),
                );
            }
            add(
                "OEBPS/content.opf",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>No TOC Book</dc:title><dc:language>en</dc:language><dc:identifier id="uid">urn:uuid:test-book</dc:identifier></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
                ),
            );
            add(
                "OEBPS/toc.xhtml",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>Contents</title></head><body><h1>Contents</h1><ul>{toc_links}</ul></body></html>"#
                ),
            );
            zip.finish().unwrap();
        }
        buf
    }

    /// The headline case: a no-TOC EPUB is repaired. `propose_toc` finds the 6
    /// chapters, `set_toc` synthesizes a nav doc + NCX (registered in the OPF),
    /// and the result opens with a real 6-entry TOC that validates OK.
    #[test]
    fn repairs_a_no_toc_epub() {
        let epub = no_toc_epub();

        // Originally deficient: a chapterless declared TOC with in-book chapters.
        let before = crate::validate::source::toc::validate(&epub).expect("validate");
        assert_eq!(before.nav_count, 0, "starts with no declared TOC");

        let proposed = propose_toc(&epub).expect("propose");
        let labels: Vec<&str> = proposed.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Chapter 1",
                "Chapter 2",
                "Chapter 3",
                "Chapter 4",
                "Chapter 5",
                "Chapter 6"
            ]
        );
        assert_eq!(proposed[0].href, "OEBPS/c1.xhtml");

        let out = repair_toc(&epub).expect("repair");

        // Reopens as a Book with the 6-chapter TOC.
        let book = Book::from_bytes(&out, Format::Epub).expect("repaired EPUB opens");
        let toc_titles: Vec<&str> = book.toc().iter().map(|e| e.title.as_str()).collect();
        assert_eq!(
            toc_titles,
            [
                "Chapter 1",
                "Chapter 2",
                "Chapter 3",
                "Chapter 4",
                "Chapter 5",
                "Chapter 6"
            ]
        );

        // And the validator now passes.
        let after = crate::validate::source::toc::validate(&out).expect("validate");
        assert_eq!(after.verdict, crate::validate::source::toc::Verdict::Ok);
    }

    /// A synthesized nav document is registered in the OPF manifest, so a reader
    /// finds it. `no_toc_epub` declares version 3.0, which is entitled to a nav
    /// document and to no NCX.
    #[test]
    fn a_synthesized_nav_doc_is_registered_in_the_opf() {
        let out = repair_toc(&no_toc_epub()).expect("repair");
        let pkg = EpubPackage::parse(&out).expect("parse");
        assert!(pkg.contains("OEBPS/nav.xhtml"), "nav doc created");
        let opf = String::from_utf8(pkg.opf_bytes().unwrap().to_vec()).unwrap();
        assert!(opf.contains("properties=\"nav\""), "nav registered");
    }

    /// The NCX branch of the same registration, on the version that asks for one.
    #[test]
    fn a_synthesized_ncx_is_registered_in_the_opf_and_spine() {
        let out = repair_toc(&epub_with_broken_anchors(
            "2.0",
            r#"<dc:identifier id="uid">urn:uuid:registered</dc:identifier>"#,
        ))
        .expect("repair");
        let pkg = EpubPackage::parse(&out).expect("parse");
        assert!(pkg.contains("OEBPS/toc.ncx"), "NCX created");
        let opf = String::from_utf8(pkg.opf_bytes().unwrap().to_vec()).unwrap();
        assert!(opf.contains("application/x-dtbncx+xml"), "NCX registered");
        assert!(opf.contains("<spine toc=\""), "spine points at the NCX");
    }

    /// On a book that already has a nav doc + NCX (the fixture), `set_toc`
    /// splices over them in place — the result reopens with exactly our entries.
    #[test]
    fn set_toc_splices_existing_docs() {
        let epub = std::fs::read(FIXTURE).expect("read fixture");
        let entries = vec![
            TocEntry::new("第一章", "OEBPS/c16.xhtml"),
            TocEntry::new("第二章", "OEBPS/c2A.xhtml"),
            TocEntry::new("第三章", "OEBPS/c5S.xhtml"),
        ];
        let out = set_toc(&epub, &entries).expect("set_toc");

        let book = Book::from_bytes(&out, Format::Epub).expect("opens");
        let titles: Vec<&str> = book.toc().iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["第一章", "第二章", "第三章"]);
        // No duplicate nav docs were created — the existing ones were reused.
        let pkg = EpubPackage::parse(&out).expect("parse");
        assert!(pkg.contains("OEBPS/nav.xhtml") && pkg.contains("OEBPS/toc.ncx"));
    }

    /// Nesting round-trips: a Part with child chapters comes back nested.
    #[test]
    fn set_toc_preserves_nesting() {
        let epub = std::fs::read(FIXTURE).expect("read fixture");
        let entries = vec![TocEntry {
            title: "第一部".into(),
            href: "OEBPS/c16.xhtml".into(),
            children: vec![
                TocEntry::new("第一章", "OEBPS/c2A.xhtml"),
                TocEntry::new("第二章", "OEBPS/c5S.xhtml"),
            ],
            play_order: None,
            target: None,
        }];
        let out = set_toc(&epub, &entries).expect("set_toc");
        let book = Book::from_bytes(&out, Format::Epub).expect("opens");
        let toc = book.toc();
        assert_eq!(toc.len(), 1);
        assert_eq!(toc[0].title, "第一部");
        let kids: Vec<&str> = toc[0].children.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(kids, ["第一章", "第二章"]);
    }

    /// Build an EPUB whose 目次 and chapter headings are images (no links, no
    /// heading text) but which ships a real NCX listing the chapters — the shape
    /// of image-heavy JP light novels.
    fn image_toc_and_ncx_epub() -> Vec<u8> {
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
            // 目次 page: an image, no links.
            add(
                "OEBPS/Text/mokuji.xhtml",
                r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>目次</title></head><body class="contents"><div><p><img src="../Images/toc.jpg" alt=""/></p></div></body></html>"#,
            );
            let mut manifest = String::from(
                r#"<item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/><item id="mokuji" href="Text/mokuji.xhtml" media-type="application/xhtml+xml"/>"#,
            );
            let mut spine = String::from(r#"<itemref idref="mokuji"/>"#);
            let mut navpoints = String::from(
                r#"<navPoint id="np0" playOrder="1"><navLabel><text>目次</text></navLabel><content src="Text/mokuji.xhtml"/></navPoint>"#,
            );
            let titles = [
                "残酷童話　うつくし姫",
                "第零話　あせろら",
                "第零話　かれん",
                "第零話　つばさ",
                "あとがき",
            ];
            for (i, title) in titles.iter().enumerate() {
                let n = i + 1;
                manifest.push_str(&format!(
                    r#"<item id="c{n}" href="Text/c{n}.xhtml" media-type="application/xhtml+xml"/>"#
                ));
                spine.push_str(&format!(r#"<itemref idref="c{n}"/>"#));
                navpoints.push_str(&format!(
                    r#"<navPoint id="np{n}" playOrder="{}"><navLabel><text>{title}</text></navLabel><content src="Text/c{n}.xhtml#link{n}"/></navPoint>"#,
                    n + 1
                ));
                // Chapter heading is an image — no text to derive a label from.
                add(
                    &format!("OEBPS/Text/c{n}.xhtml"),
                    &format!(
                        r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>c{n}</title></head><body class="image-page"><div class="main"><h2 id="link{n}"><img src="../Images/p{n}.jpg" alt=""/></h2></div></body></html>"#
                    ),
                );
            }
            add(
                "OEBPS/content.opf",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>業物語</dc:title><dc:language>ja</dc:language><dc:identifier id="uid">urn:uuid:image-toc</dc:identifier></metadata><manifest>{manifest}</manifest><spine toc="ncx">{spine}</spine><guide><reference type="toc" title="目次" href="Text/mokuji.xhtml"/></guide></package>"#
                ),
            );
            add(
                "OEBPS/toc.ncx",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1"><head><meta name="dtb:uid" content="urn:uuid:image-toc"/></head><docTitle><text>業物語</text></docTitle><navMap>{navpoints}</navMap></ncx>"#
                ),
            );
            zip.finish().unwrap();
        }
        buf
    }

    /// A 合本版 whose NCX flattens two volumes and their chapters to one depth.
    /// Each volume opens with its own full-bleed cover page (which the TOC
    /// points at) and carries its own Contents page; the colophon at the end
    /// belongs to no volume.
    fn flat_omnibus_epub(volumes: usize) -> Vec<u8> {
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
            let page = |title: &str, body: &str| {
                format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>{title}</title></head><body>{body}</body></html>"#
                )
            };
            let mut manifest = String::new();
            let mut spine = String::new();
            let mut navpoints = String::new();
            let mut order = 0;
            let mut declare = |id: &str, file: &str, label: Option<&str>| {
                manifest.push_str(&format!(
                    r#"<item id="{id}" href="{file}" media-type="application/xhtml+xml"/>"#
                ));
                spine.push_str(&format!(r#"<itemref idref="{id}"/>"#));
                if let Some(label) = label {
                    order += 1;
                    navpoints.push_str(&format!(
                        r#"<navPoint id="np{order}" playOrder="{order}"><navLabel><text>{label}</text></navLabel><content src="{file}"/></navPoint>"#
                    ));
                }
            };

            // The book's own cover — a full-bleed image page that starts no
            // volume, and is never in the TOC.
            add(
                "OEBPS/cover.xhtml",
                &page("Cover", r#"<div><img src="cover.jpg" alt=""/></div>"#),
            );
            declare("cover", "cover.xhtml", None);

            // Book-level Contents page, linking each volume's cover.
            let vol_links: String = (1..=volumes)
                .map(|v| format!(r#"<a href="v{v}cover.xhtml">Volume {v}</a>"#))
                .collect();
            add("OEBPS/contents.xhtml", &page("Contents", &vol_links));
            declare("contents", "contents.xhtml", Some("Contents"));

            for v in 1..=volumes {
                add(
                    &format!("OEBPS/v{v}cover.xhtml"),
                    &page("", &format!(r#"<div><img src="v{v}.jpg" alt=""/></div>"#)),
                );
                declare(
                    &format!("v{v}cover"),
                    &format!("v{v}cover.xhtml"),
                    Some(&format!("Volume {v}")),
                );
                let chapter_links: String = (1..=3)
                    .map(|c| format!(r#"<a href="v{v}c{c}.xhtml">Chapter {c}</a>"#))
                    .collect();
                add(&format!("OEBPS/v{v}toc.xhtml"), &page("", &chapter_links));
                declare(
                    &format!("v{v}toc"),
                    &format!("v{v}toc.xhtml"),
                    Some("Contents"),
                );
                for c in 1..=3 {
                    add(
                        &format!("OEBPS/v{v}c{c}.xhtml"),
                        &page("", &format!("<p>Volume {v}, chapter {c} prose.</p>")),
                    );
                    declare(
                        &format!("v{v}c{c}"),
                        &format!("v{v}c{c}.xhtml"),
                        Some(&format!("Chapter {c}")),
                    );
                }
            }

            // Back matter: no volume's Contents page lists it.
            add(
                "OEBPS/colophon.xhtml",
                &page("", "<p>Published by Someone.</p>"),
            );
            declare("colophon", "colophon.xhtml", Some("Colophon"));

            add(
                "OEBPS/content.opf",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Omnibus</dc:title><dc:language>en</dc:language><dc:identifier id="uid">urn:uuid:omnibus</dc:identifier></metadata><manifest><item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>{manifest}</manifest><spine toc="ncx">{spine}</spine></package>"#
                ),
            );
            add(
                "OEBPS/toc.ncx",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1"><head><meta name="dtb:uid" content="urn:uuid:omnibus"/></head><docTitle><text>Omnibus</text></docTitle><navMap>{navpoints}</navMap></ncx>"#
                ),
            );
            zip.finish().unwrap();
        }
        buf
    }

    /// A flattened multi-volume TOC is re-parented into its volumes: each
    /// volume adopts the entries its own Contents page names, nothing is lost,
    /// and back matter no volume claims stays at the top level.
    #[test]
    fn a_flattened_omnibus_toc_is_nested_back_into_its_volumes() {
        let proposed = propose_toc(&flat_omnibus_epub(2)).unwrap();

        let top: Vec<&str> = proposed.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(top, ["Contents", "Volume 1", "Volume 2", "Colophon"]);
        // Each volume adopts its own Contents page + its three chapters.
        assert_eq!(proposed[1].children.len(), 4);
        assert_eq!(proposed[2].children.len(), 4);
        assert_eq!(proposed[1].children[1].title, "Chapter 1");
        // Nothing is lost — 1 + 2×(1 + 4) + 1 declared entries come back.
        assert_eq!(flat_count(&proposed), 12);
        // The colophon belongs to no volume and is not swallowed by the last.
        assert!(proposed[3].children.is_empty());
    }

    /// Two entries on one target keep distinct navPoint ids but share a
    /// playOrder — the NCX rule epubcheck fails as RSC-005. Real books repeat a
    /// target (a duplicated 目次 row; a volume whose first chapter starts on the
    /// volume's own page).
    #[test]
    fn repeated_ncx_targets_share_one_play_order() {
        let entries = vec![
            TocEntry::new("One", "OEBPS/c1.xhtml"),
            TocEntry::new("One again", "OEBPS/c1.xhtml"),
            TocEntry::new("Two", "OEBPS/c2.xhtml"),
        ];
        let ncx = render_navmap(&entries, "OEBPS/");
        assert!(
            ncx.contains(r#"<navPoint id="navPoint-1" playOrder="1">"#),
            "{ncx}"
        );
        assert!(
            ncx.contains(r#"<navPoint id="navPoint-2" playOrder="1">"#),
            "{ncx}"
        );
        assert!(
            ncx.contains(r#"<navPoint id="navPoint-3" playOrder="2">"#),
            "{ncx}"
        );
    }

    /// Depth is whatever the book encodes — there is no level ceiling. Six
    /// levels of label indentation come back six deep, and the levels survive
    /// the nav/NCX round-trip.
    #[test]
    fn indentation_nests_to_any_depth() {
        const DEPTH: usize = 6;
        let mut entries = Vec::new();
        for level in 0..DEPTH {
            // Enough entries per level to clear the evidence threshold.
            for n in 0..crate::model::toc_shape::MIN_INDENT_EVIDENCE {
                entries.push(TocEntry::new(
                    format!("{}L{level}#{n}", "\u{3000}".repeat(level)),
                    format!("OEBPS/l{level}_{n}.xhtml"),
                ));
            }
        }
        let total = entries.len();
        let nested = nest_levels(entries, &[]);

        // Every level is one deeper than the last, and nothing was dropped.
        assert_eq!(depth(&nested), DEPTH);
        assert_eq!(flat_count(&nested), total);
        // The indentation is gone from the labels — the nesting now says it.
        assert!(!nested[0].title.starts_with('\u{3000}'));
        // Each level opens under the last entry of the one above it, so the
        // deepest chain is `last()` all the way down.
        let mut node = nested.last().expect("a root");
        for level in 1..DEPTH {
            node = node.children.last().expect("each level has children");
            assert!(
                node.title.starts_with(&format!("L{level}")),
                "{}",
                node.title
            );
        }

        // Writing and re-reading keeps the depth (the NCX carries it too).
        let epub = set_toc(&no_toc_epub(), &nested).unwrap();
        let pkg = EpubPackage::parse(&epub).unwrap();
        let opf_path = pkg.opf_path().unwrap();
        let opf_str = decode_text(pkg.opf_bytes().unwrap(), None);
        let opf = parse_opf(&opf_str).unwrap();
        let reread = existing_declared_toc(&pkg, &opf, &dir_of(&opf_path));
        assert_eq!(depth(&reread), DEPTH);
    }

    /// The audit's measurement is the repair's own rule: what
    /// `declared_toc_flattening` counts is exactly what `propose_toc`
    /// re-parents, so a diagnosis and its fix can't disagree.
    #[test]
    fn flattening_measures_what_the_repair_would_re_parent() {
        let epub = flat_omnibus_epub(2);
        let measured = declared_toc_flattening(&epub).unwrap();
        assert_eq!(measured.volumes, 2);
        assert_eq!(measured.misplaced, 8);

        let proposed = propose_toc(&epub).unwrap();
        assert_eq!(flat_count(&proposed) - proposed.len(), measured.misplaced);
        assert_eq!(
            proposed.iter().filter(|e| !e.children.is_empty()).count(),
            measured.volumes
        );

        // A repaired book measures zero — the loop closes.
        let repaired = repair_toc(&epub).unwrap();
        assert_eq!(declared_toc_flattening(&repaired).unwrap().misplaced, 0);
    }

    /// One volume is not a volume structure: a book with a single full-bleed
    /// cover page keeps the TOC it declared, flat.
    #[test]
    fn a_single_volume_book_is_left_flat() {
        let proposed = propose_toc(&flat_omnibus_epub(1)).unwrap();
        assert_eq!(flat_count(&proposed), proposed.len());
        assert!(proposed.iter().all(|e| e.children.is_empty()));
    }

    /// An ordinary novel: a title page of two publisher logos and a plate, both
    /// pages of pictures the TOC points at, and a back-matter list of the
    /// author's other books that reads as link-dense as any Contents page. Two
    /// starts, but only the title page's span holds that list, so only one
    /// group can form — and a book is not a collection of one. Counting starts
    /// instead of groups nested the whole novel under its own title page.
    fn novel_with_a_pictorial_title_page() -> Vec<u8> {
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
            let page = |body: &str| {
                format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>A Novel</title></head><body>{body}</body></html>"#
                )
            };
            let docs: Vec<(&str, String, Option<&str>)> = vec![
                (
                    "cover.xhtml",
                    page(r#"<img src="cover.jpg" alt=""/>"#),
                    None,
                ),
                (
                    "title.xhtml",
                    page(
                        r#"<p><img src="logo.jpg" alt=""/></p><p><img src="press.jpg" alt=""/></p>"#,
                    ),
                    Some("Title Page"),
                ),
                (
                    "alsoby.xhtml",
                    page(
                        r#"<a href="c1.xhtml">A Novel</a><a href="c2.xhtml">Another Novel</a><a href="c3.xhtml">A Third</a>"#,
                    ),
                    Some("Also by the Author"),
                ),
                ("c1.xhtml", page("<p>One.</p>"), Some("1 The First")),
                ("c2.xhtml", page("<p>Two.</p>"), Some("2 The Second")),
                (
                    "plate.xhtml",
                    page(r#"<img src="map.jpg" alt=""/>"#),
                    Some("Map"),
                ),
                ("c3.xhtml", page("<p>Three.</p>"), Some("3 The Third")),
            ];
            let mut manifest = String::new();
            let mut spine = String::new();
            let mut navpoints = String::new();
            for (order, (file, body, label)) in docs.iter().enumerate() {
                add(&format!("OEBPS/{file}"), body);
                manifest.push_str(&format!(
                    r#"<item id="d{order}" href="{file}" media-type="application/xhtml+xml"/>"#
                ));
                spine.push_str(&format!(r#"<itemref idref="d{order}"/>"#));
                if let Some(label) = label {
                    navpoints.push_str(&format!(
                        r#"<navPoint id="np{order}" playOrder="{order}"><navLabel><text>{label}</text></navLabel><content src="{file}"/></navPoint>"#
                    ));
                }
            }
            add(
                "OEBPS/content.opf",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>A Novel</dc:title><dc:language>en</dc:language><dc:identifier id="uid">urn:uuid:novel</dc:identifier></metadata><manifest><item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>{manifest}</manifest><spine toc="ncx">{spine}</spine></package>"#
                ),
            );
            add(
                "OEBPS/toc.ncx",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1"><head><meta name="dtb:uid" content="urn:uuid:novel"/></head><docTitle><text>A Novel</text></docTitle><navMap>{navpoints}</navMap></ncx>"#
                ),
            );
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn a_book_that_forms_a_single_group_is_left_flat() {
        let proposed = propose_toc(&novel_with_a_pictorial_title_page()).unwrap();
        assert_eq!(
            proposed
                .iter()
                .map(|e| e.title.as_str())
                .collect::<Vec<_>>(),
            [
                "Title Page",
                "Also by the Author",
                "1 The First",
                "2 The Second",
                "Map",
                "3 The Third",
            ],
            "a novel is not nested under its own title page"
        );
        assert!(proposed.iter().all(|e| e.children.is_empty()));
    }

    /// The regression: a book with an image 目次 and image chapter headings (no
    /// links, no heading text) still proposes its chapters — from the declared
    /// NCX — instead of coming up empty. Repair then writes them so it reopens
    /// with a real TOC.
    #[test]
    fn proposes_from_declared_ncx_when_body_is_images() {
        let epub = image_toc_and_ncx_epub();

        // The Contents-page / headings paths find nothing here.
        assert!(
            contents_page_links(
                &EpubPackage::parse(&epub).unwrap(),
                &parse_opf(&decode_text(
                    EpubPackage::parse(&epub).unwrap().opf_bytes().unwrap(),
                    None
                ))
                .unwrap(),
                "OEBPS/",
                ""
            )
            .is_empty(),
            "image 目次 yields no link cluster"
        );

        // …but the proposer recovers the 5 chapters from the NCX.
        let proposed = propose_toc(&epub).expect("propose");
        let labels: Vec<&str> = proposed.iter().map(|e| e.title.as_str()).collect();
        assert!(labels.contains(&"残酷童話　うつくし姫"), "got {labels:?}");
        assert!(labels.contains(&"あとがき"), "got {labels:?}");
        assert!(
            proposed
                .iter()
                .any(|e| e.href == "OEBPS/Text/c1.xhtml#link1"),
            "href resolved to an absolute zip path: {:?}",
            proposed.iter().map(|e| &e.href).collect::<Vec<_>>()
        );

        // Repair writes it back; the book reopens with the chapter list.
        let out = repair_toc(&epub).expect("repair");
        let book = Book::from_bytes(&out, Format::Epub).expect("opens");
        assert!(
            book.toc().iter().any(|e| e.title.contains("うつくし")),
            "repaired TOC lists the chapters"
        );
    }

    #[test]
    fn empty_toc_is_rejected() {
        let epub = no_toc_epub();
        assert!(set_toc(&epub, &[]).is_err());
    }

    #[test]
    fn non_epub_bytes_error() {
        assert!(propose_toc(b"not an epub").is_err());
        assert!(repair_toc(b"not an epub").is_err());
    }

    #[test]
    fn nav_tag_toc_detection_respects_boundaries() {
        assert!(nav_tag_is_toc(r#"<nav epub:type="toc""#));
        assert!(nav_tag_is_toc(r#"<nav type="toc""#));
        assert!(nav_tag_is_toc(r#"<nav epub:type="landmarks toc""#));
        assert!(!nav_tag_is_toc(r#"<nav epub:type="landmarks""#));
        assert!(!nav_tag_is_toc(r#"<nav epub:type="page-list""#));
    }

    #[test]
    fn splice_replaces_only_the_toc_nav() {
        let doc = "<body>\n<nav epub:type=\"toc\"><ol><li>old</li></ol></nav>\n<nav epub:type=\"landmarks\"><ol><li>keep</li></ol></nav>\n</body>";
        let out = splice_toc_nav(doc, "<nav epub:type=\"toc\">NEW</nav>");
        assert!(out.contains("<nav epub:type=\"toc\">NEW</nav>"));
        assert!(out.contains("landmarks"), "landmarks nav preserved");
        assert!(!out.contains("old"), "old toc nav gone");
    }
}
