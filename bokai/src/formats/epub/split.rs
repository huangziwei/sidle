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
//!
//! [`split`] then writes the volumes. It works at the zip layer rather than
//! through the IR: content documents are copied across byte for byte, and only
//! the three documents that describe the book — the OPF, the nav doc, the NCX —
//! are written fresh for each volume. Nothing is re-rendered, so a volume reads
//! exactly as its pages did inside the collection.

use std::collections::{BTreeSet, HashSet};
use std::io;

use crate::formats::epub::edit::{EpubPackage, escape_attr, escape_text};
use crate::formats::epub::nav_doc::{
    depth, render_nav_doc, render_navmap, render_ncx, render_toc_nav,
};
use crate::formats::epub::page_shape::image_only_source;
use crate::formats::epub::structure::{
    MIN_SECTION_CONTENTS_LINKS, basename, dir_of, internal_links, relativize, resolve_href,
    spine_documents, strip_fragment,
};
use crate::formats::epub::{OpfData, parse_opf, toc_repair};
use crate::model::TocEntry;
use crate::util::{decode_text, detect_mime_type, extract_xml_encoding, uuid_v5};

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
    /// volume that opens on text.
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

/// Write one EPUB per cut, in the same order.
///
/// `series`, when given, is the collection's name and goes into every volume
/// identically — a library groups by that string, so it is decided once here
/// rather than re-derived per volume. Each volume's position in it is
/// [`Cut::number`].
///
/// Every volume is self-contained: its own spine range, the resources those
/// documents reach and no others, a chapter list carved from the collection's,
/// and its own identifier. A link into a document that ended up in a different
/// volume keeps its text and loses its target — a dangling one would be a
/// broken reference in every reader that checks.
pub fn split(epub_bytes: &[u8], cuts: &[Cut], series: Option<&str>) -> io::Result<Vec<Vec<u8>>> {
    let pkg = EpubPackage::parse(epub_bytes)?;
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let opf_str = decode_text(pkg.opf_bytes()?, extract_xml_encoding(pkg.opf_bytes()?));
    let opf = parse_opf(&opf_str).map_err(io::Error::other)?;
    let spine = spine_documents(&opf, &opf_base);
    let toc = toc_repair::propose_from_pkg(&pkg, &opf, &opf_base, &opf_str);

    let mut out = Vec::with_capacity(cuts.len());
    for cut in cuts {
        let range = cut.spine_index..(cut.spine_index + cut.documents).min(spine.len());
        out.push(volume(
            &pkg, &opf, &opf_path, &opf_base, &spine, &toc, cut, range, series,
        )?);
    }
    Ok(out)
}

/// One volume as EPUB bytes.
#[allow(clippy::too_many_arguments)]
fn volume(
    pkg: &EpubPackage,
    opf: &OpfData,
    opf_path: &str,
    opf_base: &str,
    spine: &[(String, String)],
    toc: &[TocEntry],
    cut: &Cut,
    range: std::ops::Range<usize>,
    series: Option<&str>,
) -> io::Result<Vec<u8>> {
    let documents: Vec<String> = spine[range].iter().map(|(abs, _)| abs.clone()).collect();
    let kept: HashSet<&str> = documents.iter().map(String::as_str).collect();
    let resources = reachable_resources(pkg, &documents, &kept);

    // Only what the volume actually carries can collide with the two documents
    // written for it; the collection's own nav and NCX are not among them.
    let taken = |name: &str| kept.contains(name) || resources.contains(name) || name == opf_path;
    let nav_path = free_path(&taken, opf_base, "nav.xhtml");
    let ncx_path = free_path(&taken, opf_base, "toc.ncx");
    let uid = format!(
        "urn:uuid:{}",
        uuid_v5(&format!("{}#{}", opf.metadata.identifier, cut.number))
    );
    let lang = if opf.metadata.language.is_empty() {
        "en"
    } else {
        &opf.metadata.language
    };

    let mut volume = pkg.subset(|name| {
        name == "mimetype"
            || name == "META-INF/container.xml"
            || name == opf_path
            || kept.contains(name)
            || resources.contains(name)
    });
    // A cross-volume link would dangle, so it keeps its text and loses its href.
    for doc in &documents {
        if let Some(bytes) = volume.get(doc)
            && let Some(rewritten) = unlink_outside(bytes, &dir_of(doc), &kept)
        {
            volume.replace(doc, rewritten);
        }
    }
    volume.replace(
        opf_path,
        volume_opf(
            opf,
            opf_base,
            &uid,
            cut,
            series,
            &documents,
            &resources,
            &nav_path,
            &ncx_path,
            cover_image(pkg, cut),
        )
        .into_bytes(),
    );

    // The volume is a complete book by this point, so its chapter list is read
    // the same way any book's is.
    let entries = volume_toc(&volume, opf_path, carve(toc, &kept));
    volume.set(
        &nav_path,
        render_nav_doc(
            &render_toc_nav(&entries, &dir_of(&nav_path)),
            lang,
            &cut.label,
        )
        .into_bytes(),
    );
    volume.set(
        &ncx_path,
        render_ncx(
            &render_navmap(&entries, &dir_of(&ncx_path)),
            &uid,
            &cut.label,
            depth(&entries).max(1),
        )
        .into_bytes(),
    );
    volume.into_bytes()
}

/// The volume's chapter list: what the collection's own navigation said about
/// this volume, plus what the volume evidences about itself.
///
/// The collection's list is almost never enough on its own. A collection that
/// names its volumes and nothing else — the common shape — leaves each volume a
/// chapter list of exactly one row, its own title, which is no more use to a
/// reader than none. The volume's pages know better: its own Contents page, or
/// its headings, name its chapters, and that is the same derivation
/// [`super::toc_repair::propose_toc`] performs for a book with a deficient TOC.
/// Neither source is dropped — a chapter the collection named survives even if
/// the volume's own pages never mention it.
fn volume_toc(volume: &EpubPackage, opf_path: &str, carved: Vec<TocEntry>) -> Vec<TocEntry> {
    let Ok(bytes) = volume.opf_bytes() else {
        return carved;
    };
    let opf_str = decode_text(bytes, extract_xml_encoding(bytes));
    let Ok(opf) = parse_opf(&opf_str) else {
        return carved;
    };
    let opf_base = dir_of(opf_path);
    // Reads only the volume's own pages: the nav doc and NCX its package
    // declares are the ones about to be written from this, and are not there
    // yet.
    let derived = toc_repair::propose_from_pkg(volume, &opf, &opf_base, &opf_str);
    if derived.is_empty() {
        return carved;
    }
    let spine = spine_documents(&opf, &opf_base);
    let at: std::collections::HashMap<&str, usize> = spine
        .iter()
        .enumerate()
        .map(|(i, (abs, _))| (abs.as_str(), i))
        .collect();
    crate::model::toc_shape::merge_by_document_order(carved, derived, |e| {
        at.get(strip_fragment(&e.href)).copied()
    })
}

/// A zip path under `dir` that `taken` does not already claim.
fn free_path(taken: &impl Fn(&str) -> bool, dir: &str, preferred: &str) -> String {
    let candidate = format!("{dir}{preferred}");
    if !taken(&candidate) {
        return candidate;
    }
    let (stem, ext) = preferred.rsplit_once('.').unwrap_or((preferred, ""));
    (2..)
        .map(|n| format!("{dir}{stem}-{n}.{ext}"))
        .find(|p| !taken(p))
        .unwrap_or(candidate)
}

/// The chapter list restricted to the documents this volume holds, nesting kept.
/// An entry whose own target fell outside but whose children did not is dropped
/// down to its children, so a volume never loses a chapter to a heading that
/// belongs to its neighbour.
fn carve(entries: &[TocEntry], kept: &HashSet<&str>) -> Vec<TocEntry> {
    let mut out = Vec::new();
    for entry in entries {
        let children = carve(&entry.children, kept);
        if kept.contains(strip_fragment(&entry.href)) {
            out.push(TocEntry {
                children,
                ..entry.clone()
            });
        } else {
            out.extend(children);
        }
    }
    out
}

/// Every resource the volume's documents reach — images, stylesheets, and
/// whatever those stylesheets reach in turn — as absolute zip paths.
///
/// Scoping this is load-bearing rather than tidy: unscoped, every volume of a
/// collection carries the whole collection's artwork.
fn reachable_resources(
    pkg: &EpubPackage,
    documents: &[String],
    kept: &HashSet<&str>,
) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut queue: Vec<String> = documents.to_vec();
    while let Some(from) = queue.pop() {
        let Some(bytes) = pkg.get(&from) else {
            continue;
        };
        let text = decode_text(bytes, extract_xml_encoding(bytes));
        for href in references(&text) {
            let abs = resolve_href(&dir_of(&from), &href);
            let abs = strip_fragment(&abs).to_string();
            // Another spine document is a link, not a resource: which documents
            // the volume holds is the cut's business, not the markup's.
            if abs.is_empty() || kept.contains(abs.as_str()) || !pkg.contains(&abs) {
                continue;
            }
            if found.insert(abs.clone()) {
                queue.push(abs);
            }
        }
    }
    found
}

/// Every in-package reference a document or stylesheet makes: `src`, `href` and
/// `xlink:href` attributes, plus CSS `url(…)` and `@import`. External and
/// inline (`http:`, `data:`, `mailto:`) references are skipped.
fn references(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |raw: &str| {
        let value = raw.trim().trim_matches(['\'', '"']);
        if !value.is_empty() && !value.starts_with('#') && !value.contains(':') {
            out.push(value.to_string());
        }
    };
    for (attr, mut rest) in [("src=\"", text), ("href=\"", text)] {
        while let Some(p) = rest.find(attr) {
            rest = &rest[p + attr.len()..];
            match rest.find('"') {
                Some(end) => push(&rest[..end]),
                None => break,
            }
        }
    }
    let mut rest = text;
    while let Some(p) = rest.find("url(") {
        rest = &rest[p + 4..];
        match rest.find(')') {
            Some(end) => push(&rest[..end]),
            None => break,
        }
    }
    let mut rest = text;
    while let Some(p) = rest.find("@import") {
        rest = &rest[p + 7..];
        let line = rest.split(';').next().unwrap_or("");
        if !line.contains("url(") {
            push(line);
        }
    }
    out
}

/// Strip the `href` from links that leave this volume, keeping the `<a>` and
/// its text. `None` when the document has none — the common case, and the one
/// where its bytes stay exactly as the collection shipped them.
fn unlink_outside(content: &[u8], doc_dir: &str, kept: &HashSet<&str>) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(content).ok()?;
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut changed = false;
    while let Some(p) = rest.find("href=\"") {
        let (before, after) = rest.split_at(p);
        let value = &after[6..];
        let Some(end) = value.find('"') else { break };
        let href = &value[..end];
        let resolved = resolve_href(doc_dir, href);
        let target = strip_fragment(&resolved);
        // A pure `#fragment` stays inside the document; anything the volume does
        // not hold, and that is a document rather than a resource, has to go.
        let leaves = !href.starts_with('#')
            && !href.contains(':')
            && !target.is_empty()
            && !kept.contains(target)
            && target.ends_with("html");
        out.push_str(before);
        if leaves {
            changed = true;
        } else {
            out.push_str(&after[..6 + end + 1]);
        }
        rest = &after[6 + end + 1..];
    }
    if !changed {
        return None;
    }
    out.push_str(rest);
    Some(out.into_bytes())
}

/// The image a volume's cover page shows, as an absolute zip path. A page that
/// runs the cover together with a frontispiece or a title plate shows the cover
/// first, so the first image is the one that stands for the volume.
fn cover_image(pkg: &EpubPackage, cut: &Cut) -> Option<String> {
    let page = cut.cover.as_deref()?;
    let bytes = pkg.get(page)?;
    let xhtml = decode_text(bytes, extract_xml_encoding(bytes));
    let src = image_only_source(&xhtml)?;
    let abs = resolve_href(&dir_of(page), src);
    pkg.contains(&abs).then_some(abs)
}

/// The volume's package document: the collection's metadata with this volume's
/// title, identifier and place in the series, and a manifest and spine holding
/// only what the volume has.
#[allow(clippy::too_many_arguments)]
fn volume_opf(
    opf: &OpfData,
    opf_base: &str,
    uid: &str,
    cut: &Cut,
    series: Option<&str>,
    documents: &[String],
    resources: &BTreeSet<String>,
    nav_path: &str,
    ncx_path: &str,
    cover: Option<String>,
) -> String {
    let meta = &opf.metadata;
    let lang = if meta.language.is_empty() {
        "en"
    } else {
        &meta.language
    };

    let mut metadata = format!(
        "<dc:identifier id=\"uid\">{uid}</dc:identifier>\n\
         <dc:title>{}</dc:title>\n\
         <dc:language>{lang}</dc:language>\n",
        escape_text(&cut.label)
    );
    for author in &meta.authors {
        metadata.push_str(&format!(
            "<dc:creator>{}</dc:creator>\n",
            escape_text(author)
        ));
    }
    if let Some(publisher) = &meta.publisher {
        metadata.push_str(&format!(
            "<dc:publisher>{}</dc:publisher>\n",
            escape_text(publisher)
        ));
    }
    if let Some(date) = &meta.date {
        metadata.push_str(&format!("<dc:date>{}</dc:date>\n", escape_text(date)));
    }
    metadata.push_str(&format!(
        "<meta property=\"dcterms:modified\">{}</meta>\n",
        crate::util::time_now_iso8601_utc()
    ));
    if let Some(series) = series {
        metadata.push_str(&format!(
            "<meta property=\"belongs-to-collection\" id=\"series\">{}</meta>\n\
             <meta refines=\"#series\" property=\"collection-type\">series</meta>\n\
             <meta refines=\"#series\" property=\"group-position\">{}</meta>\n",
            escape_text(series),
            trim_number(cut.number)
        ));
    }
    if let Some(mode) = &meta.primary_writing_mode {
        metadata.push_str(&format!(
            "<meta name=\"primary-writing-mode\" content=\"{}\"/>\n",
            escape_attr(mode)
        ));
    }

    // The manifest names the nav doc, the NCX, the volume's documents and the
    // resources they reach — nothing else, so no manifest entry points at a
    // file the volume does not carry.
    let mut manifest = format!(
        "<item id=\"nav\" href=\"{}\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n\
         <item id=\"ncx\" href=\"{}\" media-type=\"application/x-dtbncx+xml\"/>\n",
        escape_attr(&relativize(opf_base, nav_path)),
        escape_attr(&relativize(opf_base, ncx_path))
    );
    let mut spine = String::new();
    for (n, abs) in documents.iter().enumerate() {
        manifest.push_str(&format!(
            "<item id=\"d{n}\" href=\"{}\" media-type=\"application/xhtml+xml\"/>\n",
            escape_attr(&relativize(opf_base, abs))
        ));
        spine.push_str(&format!("<itemref idref=\"d{n}\"/>\n"));
    }
    for (n, abs) in resources.iter().enumerate() {
        let cover_property = match cover.as_deref() {
            Some(c) if c == abs => " properties=\"cover-image\"",
            _ => "",
        };
        manifest.push_str(&format!(
            "<item id=\"r{n}\" href=\"{}\" media-type=\"{}\"{cover_property}/>\n",
            escape_attr(&relativize(opf_base, abs)),
            media_type(abs)
        ));
    }

    let ppd = meta
        .page_progression_direction
        .as_deref()
        .map(|d| format!(" page-progression-direction=\"{}\"", escape_attr(d)))
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"uid\" xml:lang=\"{lang}\">\n\
<metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n{metadata}</metadata>\n\
<manifest>\n{manifest}</manifest>\n\
<spine toc=\"ncx\"{ppd}>\n{spine}</spine>\n\
</package>\n"
    )
}

/// A volume number as a series position: `3` rather than `3`, `5.5` intact.
fn trim_number(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{n:.0}")
    } else {
        format!("{n}")
    }
}

/// The media type a manifest has to declare for a resource, from its name and
/// nothing else — the bytes are being copied across unread.
fn media_type(abs: &str) -> &'static str {
    let name = basename(abs);
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("css") => "text/css",
        Some("xhtml" | "html" | "htm") => "application/xhtml+xml",
        Some("ncx") => "application/x-dtbncx+xml",
        Some("js") => "text/javascript",
        Some("svg") => "image/svg+xml",
        Some("otf") => "font/otf",
        Some("ttf") => "font/ttf",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        _ => detect_mime_type(&name, &[]).unwrap_or("application/octet-stream"),
    }
}

/// A candidate volume start: a top-level entry of the repaired chapter list,
/// resolved to the spine document it opens.
struct Candidate<'a> {
    spine_index: usize,
    label: &'a str,
}

/// A chosen volume start: the candidate it is, and the number its label stated
/// if the evidence that chose it read one.
struct Start {
    candidate: usize,
    number: Option<f64>,
}

impl Start {
    /// A start chosen by the shape of its page, which says nothing about which
    /// volume it is.
    fn unnumbered(candidate: usize) -> Self {
        Start {
            candidate,
            number: None,
        }
    }
}

fn cuts(pkg: &EpubPackage, opf: &OpfData, opf_base: &str, toc: &[TocEntry]) -> Vec<Cut> {
    let spine = spine_documents(opf, opf_base);
    let spine_files: HashSet<&str> = spine.iter().map(|(_, b)| b.as_str()).collect();
    let candidates = candidates(toc, &spine);
    let starts = volume_starts(
        pkg,
        &spine,
        &spine_files,
        &candidates,
        toc,
        &opf.metadata.title,
        &opf.metadata.authors,
    );
    if starts.len() < 2 {
        // One volume is not a collection, and neither is none.
        return Vec::new();
    }

    let at: Vec<usize> = starts
        .iter()
        .map(|s| candidates[s.candidate].spine_index)
        .collect();
    let first = volume_first_documents(pkg, &spine, &at);
    let mut cuts: Vec<Cut> = Vec::new();
    for (n, start) in starts.iter().enumerate() {
        let label = candidates[start.candidate].label;
        let from = first[n];
        let to = first.get(n + 1).copied().unwrap_or(spine.len());
        let (number, numbering) = number_after(cuts.last(), label, n, start.number);
        cuts.push(Cut {
            spine_index: from,
            documents: to - from,
            label: label.to_string(),
            cover: cover_page(pkg, &spine[from].0),
            number,
            numbering,
        });
    }
    cuts
}

/// Where each volume's own pages begin, as spine indices in reading order.
///
/// Almost always that is the document the chapter list names. The exception is
/// a collection whose volumes open with a cover *and* name their own Contents
/// page: a cover carries no text, so a chapter list has nothing to call it and
/// points the entry at the page after it, leaving every cover outside every
/// entry. Read literally, such a volume opens on its Contents page — and its
/// cover becomes the last page of the volume before, or, for the first volume,
/// stays behind with the collection.
///
/// Which cover goes where is asked once about the book, never per volume: one
/// volume that happens to end on a picture is an illustration, and reading it
/// as the next volume's cover would move a page out of the book it belongs to.
/// A publisher that fronts its volumes this way does it for all of them, so the
/// walk stands only if most volumes gain something by it.
fn volume_first_documents(
    pkg: &EpubPackage,
    spine: &[(String, String)],
    named: &[usize],
) -> Vec<usize> {
    let mut fronted: Vec<usize> = Vec::with_capacity(named.len());
    for &i in named {
        // Never back into the volume before — its own first document is the
        // floor — and never onto the collection's first document, which is the
        // collection's whatever shape it has.
        let floor = fronted.last().map_or(1, |&previous| previous + 1);
        let mut first = i;
        // Only a volume named by something other than a cover can be missing
        // one. Where the chapter list already names a full-bleed page it has
        // named the volume's start, and the pages before it are the previous
        // volume's however they look.
        if cover_page(pkg, &spine[i].0).is_none() {
            while first > floor && cover_page(pkg, &spine[first - 1].0).is_some() {
                first -= 1;
            }
        }
        fronted.push(first);
    }
    let moved = fronted.iter().zip(named).filter(|(f, i)| f < i).count();
    if moved * 2 > named.len() {
        fronted
    } else {
        named.to_vec()
    }
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

/// Which candidates are volume starts.
///
/// A volume announces itself either by the page it opens on or by what the
/// chapter list calls it, and the three forms are not equally telling — each is
/// read only where the one before it found nothing:
///
/// - **Its own cover** — a page of pictures. Strong, because in an ordinary
///   book nothing but a cover looks like that; but the collection as a whole
///   still has to hold up ([`reads_as_a_collection`]), since plenty of books are
///   pictures from end to end.
/// - **Its own Contents page** — a page listing what follows it. Weaker, because
///   any chapter carrying a few cross-references looks the same from outside, so
///   it is held to a stricter test ([`contents_page_starts`]).
/// - **What the list calls it** ([`named_as_a_series`]) — for the collections
///   whose volumes open on neither: a text title page shows nothing, and a
///   publisher who draws a volume's Contents page as a picture leaves no links
///   to count. Weakest, because an ordinary novel's chapters are numbered and
///   share a stem too, so it leans hardest on what each entry has under it.
fn volume_starts(
    pkg: &EpubPackage,
    spine: &[(String, String)],
    spine_files: &HashSet<&str>,
    candidates: &[Candidate<'_>],
    toc: &[TocEntry],
    title: &str,
    authors: &[String],
) -> Vec<Start> {
    let covers: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| cover_page(pkg, &spine[c.spine_index].0).is_some())
        .map(|(n, _)| n)
        .collect();
    if reads_as_a_collection(pkg, spine, spine_files, candidates, &covers) {
        return covers.into_iter().map(Start::unnumbered).collect();
    }
    let contents = contents_page_starts(pkg, spine, spine_files, candidates);
    if contents.len() >= 2 {
        return contents.into_iter().map(Start::unnumbered).collect();
    }
    named_as_a_series(spine, candidates, toc, title, authors)
}

/// The candidates the chapter list names as a series, with the numbers it
/// states — read when neither the page a volume opens on nor a Contents page of
/// its own told the volumes from the matter listed beside them.
///
/// Two ways a list names a series, and a collection uses one or the other:
///
/// - **A running count.** The labels share a stem and differ by a number that
///   climbs: `BOOK 1: …`, `BOOK 2: …`. The stem is whatever precedes the first
///   digit, so `第3巻` and `Vol. 3` read as readily as `BOOK 3` — and the number
///   is stated rather than counted, which is worth keeping.
/// - **The work's own name, repeated.** Each volume's label opens with the name
///   the book gives itself and its own subtitle follows. Held to the book's
///   `dc:title` rather than to a stem the labels merely share: twelve labels
///   reading exactly `目次` share a stem too, and none of them is the work.
///
/// Both are then held to the same thing, per entry: an entry that names a
/// volume has a volume under it ([`carries_a_volume`]). Labels alone would read
/// an ordinary novel's chapters as a series — numbered, sharing a stem — and
/// what separates them is that a chapter lists no chapters of its own.
fn named_as_a_series(
    spine: &[(String, String)],
    candidates: &[Candidate<'_>],
    toc: &[TocEntry],
    title: &str,
    authors: &[String],
) -> Vec<Start> {
    let entries = entry_positions(toc, spine);
    // A count of two is the commonest shape in publishing — every novel with a
    // Book One and a Book Two — so a count has to reach three before it reads
    // as a series. A work's name repeated needs no such margin: a book does not
    // name its own halves after itself unless they were published that way.
    for (chosen, least) in [
        (counted_labels(candidates), 3),
        (title_bearing_labels(candidates, title, authors), 2),
    ] {
        let kept = carries_a_volume(spine, candidates, &entries, chosen);
        if kept.len() >= least {
            return kept;
        }
    }
    Vec::new()
}

/// Every chapter-list entry as a spine index, its children included and in no
/// particular order — how much of the book an entry has under it is a count,
/// not a sequence.
fn entry_positions(toc: &[TocEntry], spine: &[(String, String)]) -> Vec<usize> {
    let at: std::collections::HashMap<&str, usize> = spine
        .iter()
        .enumerate()
        .map(|(i, (abs, _))| (abs.as_str(), i))
        .collect();
    let mut out = Vec::new();
    let mut stack: Vec<&TocEntry> = toc.iter().collect();
    while let Some(entry) = stack.pop() {
        if let Some(&i) = at.get(strip_fragment(&entry.href)) {
            out.push(i);
        }
        stack.extend(entry.children.iter());
    }
    out
}

/// The starts with a volume under them: the chapter list names entries inside
/// the span each one opens, beyond the start's own document.
///
/// This is what makes a label signal usable at all. A chapter is one document
/// the list names once; a volume is a run of them the list names throughout.
/// Dropping an entry only ever lengthens its neighbours' spans, so what
/// survives one pass survives.
fn carries_a_volume(
    spine: &[(String, String)],
    candidates: &[Candidate<'_>],
    entries: &[usize],
    chosen: Vec<Start>,
) -> Vec<Start> {
    let at = |s: &Start| candidates[s.candidate].spine_index;
    chosen
        .iter()
        .enumerate()
        .filter(|(n, start)| {
            let from = at(start);
            let to = chosen.get(n + 1).map_or(spine.len(), at);
            entries.iter().filter(|&&i| i > from && i < to).count() >= MIN_SECTION_CONTENTS_LINKS
        })
        .map(|(_, start)| Start {
            candidate: start.candidate,
            number: start.number,
        })
        .collect()
}

/// The largest run of candidates whose labels count volumes up from a shared
/// stem, in the book's own order.
fn counted_labels(candidates: &[Candidate<'_>]) -> Vec<Start> {
    let mut stems: Vec<(&str, Vec<Start>)> = Vec::new();
    for (n, c) in candidates.iter().enumerate() {
        let Some((stem, number)) = counted_volume(c.label) else {
            continue;
        };
        let start = Start {
            candidate: n,
            number: Some(number),
        };
        match stems.iter_mut().find(|(s, _)| *s == stem) {
            Some((_, group)) => group.push(start),
            None => stems.push((stem, vec![start])),
        }
    }
    stems
        .into_iter()
        .map(|(_, group)| group)
        // A count only counts if it climbs: a stem shared by labels whose
        // numbers wander is a coincidence of wording, not a numbering.
        .filter(|group| group.windows(2).all(|w| w[0].number < w[1].number))
        .max_by_key(Vec::len)
        .unwrap_or_default()
}

/// The words a Latin-script label numbers *volumes* with. `part`, `chapter`,
/// `section` and `act` are deliberately absent for exactly the reason 章 and 話
/// are absent from [`VOLUME_COUNTERS`]: they number the divisions of one book,
/// which is what a volume contains. This vocabulary is the whole of the
/// signal's precision — a novel in four `PART n`s and a textbook in ten `第n章`s
/// are otherwise shaped exactly like a ten-volume set.
const VOLUME_WORDS: [&str; 3] = ["book", "volume", "vol"];

/// What a label reads as a counted volume: the stem it counts from, and the
/// number. `None` unless the stem says it is counting volumes.
fn counted_volume(label: &str) -> Option<(&str, f64)> {
    // A CJK counter construction names its own counter, and `counted_number`
    // already holds it to one that counts volumes. Every such label shares a
    // stem by construction — they are all `第…`.
    if let Some(number) = counted_number(label) {
        return Some(("第", number));
    }
    let at = label.char_indices().find(|&(_, c)| is_digit(c))?.0;
    let stem = &label[..at];
    let word = stem.trim_matches(|c: char| !c.is_alphanumeric());
    if !VOLUME_WORDS.iter().any(|v| word.eq_ignore_ascii_case(v)) {
        return None;
    }
    let len: usize = label[at..]
        .chars()
        .take_while(|&c| is_numeral(c))
        .map(char::len_utf8)
        .sum();
    Some((stem, read_numerals(&label[at..at + len])?))
}

fn is_digit(c: char) -> bool {
    c.is_ascii_digit() || ('０'..='９').contains(&c)
}

/// Candidates whose labels open with the name the book gives itself.
///
/// The shortest stem that can name a work is four characters — below that a
/// stem is an article or a particle, and every label starts with one.
const MIN_WORK_NAME: usize = 4;

fn title_bearing_labels(
    candidates: &[Candidate<'_>],
    title: &str,
    authors: &[String],
) -> Vec<Start> {
    let named_by = |stem: &str| {
        candidates
            .iter()
            .filter(|c| c.label.starts_with(stem))
            .count()
    };
    // The stem that names the most entries, not the longest one. A collection's
    // own 目次 and 奥付 are labelled with the whole of its title — banner, volume
    // count and all — which is a longer stem than the volumes carry and names
    // two entries that are not volumes.
    let Some(stem) = candidates
        .iter()
        .filter_map(|c| title_borne_prefix(c.label, title))
        .filter(|stem| !names_the_author(stem, authors) && tells_volumes_apart(candidates, stem))
        .max_by_key(|stem| (named_by(stem), stem.len()))
    else {
        return Vec::new();
    };
    candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.label.starts_with(stem))
        .map(|(n, _)| Start::unnumbered(n))
        .collect()
}

/// Whether a stem is the author's name rather than the work's. A book titled
/// for whoever wrote it — an interview collection, a companion volume — puts
/// that name at the head of its own title and at the head of every section, and
/// nothing about that is a series.
fn names_the_author(stem: &str, authors: &[String]) -> bool {
    let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let stem = squash(stem);
    authors.iter().any(|a| squash(a).starts_with(&stem))
}

/// Whether `stem` picks out entries a reader could tell apart: at least two of
/// them, each saying something of its own after it.
///
/// Volumes are distinguished by what follows the work's name — a subtitle, a
/// number, an 上/下. An entry with nothing after it *is* the work rather than a
/// volume of it, and two entries saying the same thing are one thing listed
/// twice; both are what a book holding a story of its own name looks like from
/// here.
fn tells_volumes_apart(candidates: &[Candidate<'_>], stem: &str) -> bool {
    let mut rest: Vec<&str> = candidates
        .iter()
        .filter_map(|c| c.label.strip_prefix(stem))
        .collect();
    let found = rest.len();
    rest.sort_unstable();
    rest.dedup();
    found >= 2 && rest.len() == found && rest.iter().all(|r| !r.trim().is_empty())
}

/// The longest prefix `label` shares with the name the book gives itself.
///
/// Shares with the *head* of the title, not found anywhere inside it. A work's
/// name is what its title opens on; a noun from the middle of a title is a
/// coincidence, and a book whose sections are all phrased around its subject
/// will have several of them starting with that noun.
fn title_borne_prefix<'a>(label: &'a str, title: &str) -> Option<&'a str> {
    let head = title_head(title);
    let mut longest = None;
    for (i, c) in label.char_indices() {
        let prefix = &label[..i + c.len_utf8()];
        if !head.starts_with(prefix) {
            break;
        }
        longest = Some(prefix);
    }
    longest.filter(|p| names_a_work(p.trim()))
}

/// Whether a stem is long enough to be a work's name.
///
/// Four characters, in a script that runs them together. A script that
/// separates words needs two of them instead: four characters is one short
/// word there, and the word a title opens on is as often an article or an
/// interrogative — which every section of a book phrased as questions also
/// opens on.
fn names_a_work(name: &str) -> bool {
    if name.chars().count() < MIN_WORK_NAME {
        return false;
    }
    !name.chars().any(|c| c.is_ascii_alphabetic()) || name.split_whitespace().count() >= 2
}

/// The brackets a title's opening banner is written in.
const BANNER_BRACKETS: [(char, char); 6] = [
    ('【', '】'),
    ('［', '］'),
    ('[', ']'),
    ('（', '）'),
    ('(', ')'),
    ('＜', '＞'),
];

/// A title with the banners it opens on taken off — a bracketed `合本版`, a
/// volume count, a `[Boxed Set]`. A collection labels itself in front of its
/// own name, and the name is what its volumes carry.
fn title_head(title: &str) -> &str {
    let mut head = title.trim_start();
    loop {
        let Some(&(_, close)) = BANNER_BRACKETS
            .iter()
            .find(|(open, _)| head.starts_with(*open))
        else {
            return head;
        };
        let Some(at) = head.find(close) else {
            return head;
        };
        head = head[at + close.len_utf8()..].trim_start();
    }
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
                linked_documents(pkg, spine, spine_files, i).len() >= MIN_SECTION_CONTENTS_LINKS
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
            targets.len() >= MIN_SECTION_CONTENTS_LINKS
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

/// The document's own zip path when it is a page of pictures — how a volume's
/// cover is authored — and `None` when it is anything else.
fn cover_page(pkg: &EpubPackage, abs: &str) -> Option<String> {
    let bytes = pkg.get(abs)?;
    let xhtml = decode_text(bytes, extract_xml_encoding(bytes));
    image_only_source(&xhtml)?;
    Some(abs.to_string())
}

// ---------------------------------------------------------------------------
// Volume numbers
// ---------------------------------------------------------------------------

/// The number to give the volume `label` names, following `previous`.
///
/// `stated` is a number the evidence that chose this volume already read off
/// the label — [`named_as_a_series`] reads one to find the volumes at all — and
/// it stands in for what [`volume_number`] would find. Both are labels stating
/// their own number; which of them read it is not a difference worth keeping.
///
/// A label is believed only where it continues the numbering: an index that goes
/// backwards is not this volume's place in the series, it is a number that
/// happens to be in its title. A collection that runs its main line to
/// twenty-seven and then ships side stories numbered from one again is counting
/// two different things, and the volume's place in the collection is the one
/// being asked for.
fn number_after(
    previous: Option<&Cut>,
    label: &str,
    index: usize,
    stated: Option<f64>,
) -> (f64, Numbering) {
    let previous = previous.map(|c| c.number);
    match stated.or_else(|| volume_number(label)) {
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

    /// The same book, named one page later. A cover has no text to name it by,
    /// so plenty of collections point each entry at the volume's Contents page
    /// and leave the cover in front of it, named by nothing. The volume still
    /// begins at its cover: taken at face value the first volume's cover would
    /// stay with the collection and every other one would end up as the last
    /// page of the volume before it.
    #[test]
    fn a_volume_named_by_its_contents_page_still_begins_at_its_cover() {
        let mut docs: Vec<Doc<'static>> = vec![
            ("cover.xhtml", image_page("cover.jpg"), Some("表紙")),
            (
                "toc.xhtml",
                contents_page(&["v1toc.xhtml", "v2toc.xhtml", "v3toc.xhtml"]),
                Some("目次"),
            ),
        ];
        for (v, label) in [(1, "物語"), (2, "物語2"), (3, "物語3")] {
            let mut volume = volume(v);
            // The label sits on the Contents page, not on the cover before it.
            volume[1].2 = Some(label);
            docs.extend(volume);
        }
        docs.push(("colophon.xhtml", "<p>奥付</p>".into(), Some("奥付")));

        let cuts = propose_cuts(&epub(&docs)).expect("propose");
        assert_eq!(
            cuts.iter()
                .map(|c| (
                    c.spine_index,
                    c.documents,
                    c.label.as_str(),
                    c.cover.as_deref()
                ))
                .collect::<Vec<_>>(),
            [
                (2, 4, "物語", Some("OEBPS/v1.xhtml")),
                (6, 4, "物語2", Some("OEBPS/v2.xhtml")),
                (10, 5, "物語3", Some("OEBPS/v3.xhtml")),
            ],
            "the same cuts a collection naming its covers gives"
        );
    }

    /// The walk back onto a cover is a fact about the collection, not about one
    /// volume. A book whose volumes are named by their own cover already starts
    /// them in the right place, and a picture at the end of one volume is that
    /// volume's — a plate, a map, an afterword illustration — not the next
    /// volume's cover.
    #[test]
    fn a_picture_ending_a_volume_is_not_the_next_volumes_cover() {
        let mut docs = collection_documents();
        // A plate closing volume 1, immediately before volume 2's own cover.
        let at = docs
            .iter()
            .position(|(name, _, _)| *name == "v2.xhtml")
            .expect("volume 2 starts with a cover");
        docs.insert(at, ("plate.xhtml", image_page("plate.jpg"), None));

        let cuts = propose_cuts(&epub(&docs)).expect("propose");
        assert_eq!(
            cuts.iter()
                .map(|c| (c.spine_index, c.documents, c.label.as_str()))
                .collect::<Vec<_>>(),
            [(2, 5, "物語"), (7, 4, "物語2"), (11, 5, "物語3")],
            "the plate stays with the volume it closes"
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
            number_after(Some(&cut), label, index, None)
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
        assert_eq!(
            number_after(None, "無題", 0, None),
            (1.0, Numbering::Sequence)
        );
    }

    /// A number the detection already read off the label is the label stating
    /// it, and is held to the same rule: believed where it climbs, dropped
    /// where it does not.
    #[test]
    fn a_number_read_while_finding_the_volume_counts_as_the_label_stating_it() {
        let previous = Cut {
            spine_index: 0,
            documents: 1,
            label: String::new(),
            cover: None,
            number: 4.0,
            numbering: Numbering::Label,
        };
        // The label alone says nothing; the counter the detector read does.
        assert_eq!(
            number_after(Some(&previous), "BOOK 5: THE SUBTITLE", 4, Some(5.0)),
            (5.0, Numbering::Label)
        );
        assert_eq!(volume_number("BOOK 5: THE SUBTITLE"), None);
        assert_eq!(
            number_after(Some(&previous), "BOOK 2: THE SUBTITLE", 4, Some(2.0)),
            (5.0, Numbering::Sequence)
        );
    }

    /// A label counts volumes only where its own words say it is counting
    /// volumes. Everything numbers something; a part, a chapter and an act
    /// number the insides of one book.
    #[test]
    fn only_a_label_that_counts_volumes_is_read_as_counting_them() {
        assert_eq!(counted_volume("BOOK 1: THE SUBTITLE"), Some(("BOOK ", 1.0)));
        assert_eq!(counted_volume("Vol. 10.5"), Some(("Vol. ", 10.5)));
        assert_eq!(counted_volume("Volume 2"), Some(("Volume ", 2.0)));
        assert_eq!(counted_volume("第３巻"), Some(("第", 3.0)));

        // The divisions of one book, however they are spelled.
        assert_eq!(counted_volume("PART 1 THE SUBTITLE"), None);
        assert_eq!(counted_volume("Chapter 4"), None);
        assert_eq!(counted_volume("第1章　概要"), None);
        assert_eq!(counted_volume("About the Author"), None);
    }

    /// A stem is the work's name only if the book calls itself that. Every
    /// volume of a collection carries a 目次, so twelve labels reading exactly
    /// `目次` share a stem perfectly — and none of them is a volume.
    #[test]
    fn a_stem_names_the_work_only_where_the_books_own_title_carries_it() {
        let title = "【合本版】異世界の物語　全12巻 (文庫)";
        assert_eq!(
            title_borne_prefix("異世界の物語　ウサギが呼びました", title),
            Some("異世界の物語　")
        );
        assert_eq!(title_borne_prefix("目次", title), None);
        // Too short to be a name: the title contains it, but so would anything.
        assert_eq!(title_borne_prefix("全1巻のこと", title), None);
    }
}
