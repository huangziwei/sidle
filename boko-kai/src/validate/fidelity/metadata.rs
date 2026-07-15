//! Metadata round-trip validation — verify the OPF metadata fields the
//! Kindle reader exposes survive into KFX.
//!
//! Source side: parse the OPF (`Metadata` struct from boko's epub parser).
//! KFX side: walk two entities:
//!
//! - **`metadata` ($258)** — has `reading_orders[*].page_progression_direction`.
//! - **`book_metadata` ($490)** — has `categorised_metadata`, a list of
//!   `{category, metadata: [{key, value}, ...]}` blobs. The Kindle library
//!   service reads keys like `title`, `author`, `language`, `cover_image`.
//!
//! Round-trip rules:
//!
//! - `title` → KindleTitle.`title` (exact string match)
//! - `language` → KindleTitle.`language`
//! - `authors[0]` → KindleTitle.`author` (KFX only stores one; if source has
//!   multiple, this validator checks the first survives — additional authors
//!   are silently lost by design)
//! - `cover_image` (path) → must produce a non-empty `cover_image` value
//!   pointing at a resource_name (existence check only; the path
//!   transformation is intentional)
//! - `page_progression_direction` → metadata.reading_orders[0].
//!   page_progression_direction (`$rtl` / `$ltr` / omitted)

use std::collections::HashMap;
use std::io::Cursor;

use zip::ZipArchive;

use crate::epub::{parse_container_xml, parse_opf};
use crate::kfx::container::{
    SymbolTable, parse_container_header, parse_container_info, parse_index_table, skip_enty_header,
};
use crate::kfx::ion::{IonParser, IonValue};
use crate::kfx::symbols::KfxSymbol;

/// A field-level mismatch between EPUB and KFX. Direction-neutral: `epub` is
/// the value seen on the EPUB side, `kfx` is the value seen on the KFX side,
/// regardless of which one is the conversion source.
#[derive(Debug, Clone)]
pub struct FieldDiff {
    pub field: &'static str,
    pub epub: String,
    pub kfx: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub epub_title: String,
    pub epub_language: String,
    /// Full ordered author list from `<dc:creator>` elements. The first entry
    /// is the primary author per EPUB convention.
    pub epub_authors: Vec<String>,
    pub epub_identifier: String,
    /// All `<dc:identifier>` (scheme, value) pairs in the OPF (calibre emits
    /// `ASIN`, `MOBI-ASIN`, `uuid` schemes plus a `calibre` scheme).
    pub epub_identifiers: Vec<(String, String)>,
    pub epub_has_cover: bool,
    pub epub_ppd: Option<String>,
    /// `<dc:date>` value if any (ISO-8601 or whatever the source emits).
    pub epub_date: Option<String>,
    /// True iff the OPF carries `<meta name="primary-writing-mode" .../>`.
    pub epub_primary_writing_mode: Option<String>,

    /// Number of spine XHTML docs whose `<html>` root carries `xml:lang`.
    pub epub_html_lang_present: usize,
    /// Total spine doc count.
    pub epub_html_doc_count: usize,
    /// Parent-escape (`..`) usage in spine `<img src>` or stylesheet
    /// `<link href>`. Apple Books rejects these even when they resolve
    /// mathematically (because the resolution still leaves the document's
    /// container) — the result is missing images and an unloaded
    /// stylesheet, and therefore no vertical writing mode.
    pub epub_parent_escape_refs: Vec<String>,
    /// OPF `<package version>` string (e.g. `"2.0"`, `"3.0"`).
    pub epub_opf_version: String,
    /// True if `<package version>` starts with `"3"` but the manifest has
    /// no `<item properties="nav">`. EPUB 3.x mandates a nav document
    /// separate from the NCX; strict readers reject 3.x packages without
    /// one.
    pub epub_epub3_missing_nav: bool,

    pub kfx_title: String,
    pub kfx_language: String,
    /// Ordered author list as it appears in KFX `kindle_title_metadata`
    /// (repeated `author` keys). Source order — calibre's library output
    /// emits these in the same order via `<dc:creator>`.
    pub kfx_authors: Vec<String>,
    pub kfx_cover_image: Option<String>,
    pub kfx_ppd: Option<String>,
    /// `book_id` field if present — derived from EPUB identifier in EPUB→KFX
    /// flow, or already present from a prior conversion in KFX→EPUB flow.
    pub kfx_book_id: Option<String>,
    /// `ASIN` field from `kindle_title_metadata` (Amazon catalogue id).
    pub kfx_asin: Option<String>,
    /// `issue_date` field from `kindle_title_metadata` (publication date).
    pub kfx_issue_date: Option<String>,
    /// True iff any KFX style struct has `writing_mode` ending in `_rl` or
    /// `_lr` (vertical). When true, calibre emits
    /// `<meta name="primary-writing-mode">` in the OPF as a Kindle hint.
    pub kfx_is_vertical: bool,
    /// Detected vertical writing mode value (e.g. `vertical-rl`) when
    /// `kfx_is_vertical` is true.
    pub kfx_vertical_mode: Option<String>,

    pub diffs: Vec<FieldDiff>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.diffs.is_empty()
    }

    pub fn print_summary(&self, _dir: super::Direction) {
        println!("Title:");
        println!("  EPUB: {:?}", self.epub_title);
        println!("  KFX:  {:?}", self.kfx_title);
        println!("Language:");
        println!("  EPUB: {:?}", self.epub_language);
        println!("  KFX:  {:?}", self.kfx_language);
        println!("Authors (ordered):");
        println!(
            "  EPUB ({}): {:?}",
            self.epub_authors.len(),
            self.epub_authors
        );
        println!(
            "  KFX  ({}): {:?}",
            self.kfx_authors.len(),
            self.kfx_authors
        );
        println!("Cover image:");
        println!("  EPUB has cover:  {}", self.epub_has_cover);
        println!("  KFX cover_image: {:?}", self.kfx_cover_image);
        println!("Page progression direction:");
        println!("  EPUB: {:?}", self.epub_ppd);
        println!("  KFX:  {:?}", self.kfx_ppd);
        println!("Identifiers:");
        println!("  EPUB unique-id: {:?}", self.epub_identifier);
        println!("  EPUB all:       {:?}", self.epub_identifiers);
        println!("  KFX book_id:    {:?}", self.kfx_book_id);
        println!("  KFX ASIN:       {:?}", self.kfx_asin);
        println!("Publication date:");
        println!("  EPUB <dc:date>:           {:?}", self.epub_date);
        println!("  KFX  issue_date:          {:?}", self.kfx_issue_date);
        println!("Primary writing mode (Kindle hint):");
        println!(
            "  EPUB <meta primary-writing-mode>: {:?}",
            self.epub_primary_writing_mode
        );
        println!(
            "  KFX  vertical?:                   {} ({:?})",
            self.kfx_is_vertical, self.kfx_vertical_mode
        );
        println!("xml:lang on spine docs:");
        println!(
            "  {} / {} spine XHTMLs carry xml:lang on <html>",
            self.epub_html_lang_present, self.epub_html_doc_count
        );
        println!("EPUB compatibility:");
        println!("  OPF version:                {}", self.epub_opf_version);
        println!(
            "  Parent-escape (..) in spine refs: {}",
            self.epub_parent_escape_refs.len()
        );
        if self.epub_epub3_missing_nav {
            println!("  DEFECT: <package version=\"3.x\"> with no <item properties=\"nav\">");
        }
        println!("Defects: {}", self.diffs.len());
    }

    pub fn print_details(&self, limit: usize, _dir: super::Direction) {
        if !self.diffs.is_empty() {
            println!("\n--- Field mismatches ---");
            for d in &self.diffs {
                println!("  {}: epub={:?}  kfx={:?}", d.field, d.epub, d.kfx);
            }
        }
        if !self.epub_parent_escape_refs.is_empty() {
            println!(
                "\n--- Spine resource refs with `..` parent escape [first {}] ---",
                limit
            );
            for r in self.epub_parent_escape_refs.iter().take(limit) {
                println!("  {}", r);
            }
            if self.epub_parent_escape_refs.len() > limit {
                println!(
                    "  ... and {} more",
                    self.epub_parent_escape_refs.len() - limit
                );
            }
        }
    }
}

pub fn validate(
    epub_bytes: &[u8],
    kfx_bytes: &[u8],
    dir: super::Direction,
) -> Result<Report, String> {
    let epub = extract_epub_metadata(epub_bytes)?;
    let kfx = extract_kfx_metadata(kfx_bytes)?;

    let mut diffs: Vec<FieldDiff> = Vec::new();

    if !epub.title.is_empty() && epub.title != kfx.title {
        diffs.push(FieldDiff {
            field: "title",
            epub: epub.title.clone(),
            kfx: kfx.title.clone(),
        });
    }
    // Flag only when the KFX *has* a language that boko didn't carry faithfully
    // (dropped, or changed). When the KFX language is empty, boko supplying a
    // default `dc:language` is required by EPUB and isn't a fidelity defect —
    // there's nothing in the source to be unfaithful to. (boko's hard-coded "en"
    // fallback is a poor default for a CJK-heavy library, but no corpus book hits
    // it wrongly; revisit if a non-English empty-language book appears.)
    if !kfx.language.is_empty() && epub.language != kfx.language {
        diffs.push(FieldDiff {
            field: "language",
            epub: epub.language.clone(),
            kfx: kfx.language.clone(),
        });
    }
    // Authors: full ordered vector compare. KFX side preserves source order
    // (mirrors `yj_metadata.py:get_yj_metadata_from_book` which uses
    // `authors.append(val)`). EPUB side reads `<dc:creator>` elements in
    // OPF source order.
    if !kfx.authors.is_empty() && epub.authors != kfx.authors {
        diffs.push(FieldDiff {
            field: "authors (ordered)",
            epub: format!("{:?}", epub.authors),
            kfx: format!("{:?}", kfx.authors),
        });
    }
    // Cover: EPUB declares a cover path → KFX should have a non-empty
    // cover_image pointing at a resource. We don't compare paths; the
    // transformation OPF-path → KFX-resource-name is intentional.
    if epub.has_cover && kfx.cover_image.as_deref().unwrap_or("").is_empty() {
        diffs.push(FieldDiff {
            field: "cover_image",
            epub: epub.cover_path.clone().unwrap_or_default(),
            kfx: "(missing)".into(),
        });
    }
    // PPD check moved to `validate::fidelity::page_progression`, which mirrors calibre's
    // writing-mode → ppd override (a KFX with `direction: ltr` + `writing_mode:
    // vertical_rl` still has PPD = rtl, which a literal field-by-field compare
    // here would miss). PPD values are still printed below as informational.

    // The remaining checks depend on which side is boko's output. When the
    // EPUB is generated from a KFX ("port"), it must carry the KFX's
    // metadata and satisfy EPUB-product hygiene. When the EPUB is the
    // ground-truth source, those same conditions describe the *source* and
    // are not conversion defects — the mirrored checks below apply instead
    // (source hygiene is `validate source`'s job).
    let epub_is_output = !dir.epub_is_source();

    // ASIN: KFX `kindle_title_metadata.ASIN` is the Amazon catalogue id.
    // Calibre emits it as `<dc:identifier opf:scheme="ASIN">B0CPJ2B88T</...>`;
    // EPUB-3-valid output tags it `id="asin"` instead (see
    // `scan_opf_identifiers`).
    if epub_is_output {
        // If KFX has it but EPUB lacks any ASIN identifier (or no
        // identifier matching the value), that's a port defect.
        if let Some(asin) = kfx.asin.as_deref() {
            let epub_has_asin = epub
                .identifiers
                .iter()
                .any(|(scheme, value)| is_asin_identifier(scheme) && value == asin);
            if !epub_has_asin {
                diffs.push(FieldDiff {
                    field: "ASIN identifier",
                    epub: format!("{:?}", epub.identifiers),
                    kfx: asin.to_string(),
                });
            }
        }
    } else {
        // A source ASIN must survive into the KFX. An EPUB without one is
        // fine — KFX requires an ASIN, so the exporter synthesizes it.
        let epub_asin = epub
            .identifiers
            .iter()
            .find(|(scheme, _)| is_asin_identifier(scheme))
            .map(|(_, value)| value.as_str());
        if let Some(src_asin) = epub_asin
            && kfx.asin.as_deref() != Some(src_asin)
        {
            diffs.push(FieldDiff {
                field: "ASIN identifier",
                epub: src_asin.to_string(),
                kfx: kfx.asin.clone().unwrap_or_else(|| "(missing)".into()),
            });
        }
    }

    // Publication date: KFX `kindle_title_metadata.issue_date` ↔ EPUB
    // `<dc:date>`. We don't compare formats — KFX uses `YYYY-MM-DD` strings,
    // calibre normalises to ISO-8601 with offset. The defect we catch is
    // "the source has a date, boko's output has none."
    if epub_is_output {
        if kfx.issue_date.is_some() && epub.date.is_none() {
            diffs.push(FieldDiff {
                field: "dc:date",
                epub: "(missing)".into(),
                kfx: kfx.issue_date.clone().unwrap_or_default(),
            });
        }
    } else if epub.date.is_some() && kfx.issue_date.is_none() {
        diffs.push(FieldDiff {
            field: "issue_date",
            epub: epub.date.clone().unwrap_or_default(),
            kfx: "(missing)".into(),
        });
    }

    // Primary writing mode hint: calibre emits
    // `<meta name="primary-writing-mode" content="vertical-rl"/>` for
    // vertical books. The Kindle app uses it as a layout hint. When KFX
    // declares a vertical writing mode, a generated EPUB should carry the
    // meta. (In the EPUB→KFX direction, mode fidelity is covered by the
    // dedicated writing-mode validator.)
    if epub_is_output && kfx.is_vertical && epub.primary_writing_mode.is_none() {
        diffs.push(FieldDiff {
            field: "meta primary-writing-mode",
            epub: "(missing)".into(),
            kfx: kfx.vertical_mode.clone().unwrap_or_default(),
        });
    }

    // `xml:lang` on each spine XHTML `<html>` root — calibre adds it for
    // every spine doc (the per-doc language hint that reading systems use
    // for font selection and word-break behaviour). Source-of-truth is
    // KFX `language`. If any generated spine doc is missing it, that's a
    // port defect.
    if epub_is_output
        && !kfx.language.is_empty()
        && epub.html_doc_count > 0
        && epub.html_lang_present < epub.html_doc_count
    {
        diffs.push(FieldDiff {
            field: "<html xml:lang> on spine docs",
            epub: format!(
                "{} of {} carry it",
                epub.html_lang_present, epub.html_doc_count
            ),
            kfx: kfx.language.clone(),
        });
    }

    // Parent-escape (`..`) in any spine `<img src>` / `<link href>`.
    // Apple Books treats these as broken even when they mathematically
    // resolve, causing missing images and an unloaded stylesheet (which
    // suppresses vertical writing mode in CJK books). The correct path
    // for a chapter resource is sibling-relative to the chapter file.
    if epub_is_output && !epub.parent_escape_refs.is_empty() {
        diffs.push(FieldDiff {
            field: "spine resource refs with .. parent escape",
            epub: format!(
                "{} ref(s); first: {}",
                epub.parent_escape_refs.len(),
                epub.parent_escape_refs[0]
            ),
            kfx: "n/a".into(),
        });
    }

    // EPUB 3.x without nav.xhtml. Strict readers (Apple Books) reject
    // these silently — the package opens, no content renders.
    if epub_is_output && epub.epub3_missing_nav {
        diffs.push(FieldDiff {
            field: "EPUB3 missing <item properties=\"nav\">",
            epub: epub.opf_version.clone(),
            kfx: "n/a".into(),
        });
    }

    Ok(Report {
        epub_title: epub.title,
        epub_language: epub.language,
        epub_authors: epub.authors,
        epub_identifier: epub.identifier,
        epub_identifiers: epub.identifiers,
        epub_has_cover: epub.has_cover,
        epub_ppd: epub.ppd,
        epub_date: epub.date,
        epub_primary_writing_mode: epub.primary_writing_mode,
        epub_html_lang_present: epub.html_lang_present,
        epub_html_doc_count: epub.html_doc_count,
        epub_parent_escape_refs: epub.parent_escape_refs,
        epub_opf_version: epub.opf_version,
        epub_epub3_missing_nav: epub.epub3_missing_nav,
        kfx_title: kfx.title,
        kfx_language: kfx.language,
        kfx_authors: kfx.authors,
        kfx_cover_image: kfx.cover_image,
        kfx_ppd: kfx.ppd,
        kfx_book_id: kfx.book_id,
        kfx_asin: kfx.asin,
        kfx_issue_date: kfx.issue_date,
        kfx_is_vertical: kfx.is_vertical,
        kfx_vertical_mode: kfx.vertical_mode,
        diffs,
    })
}

/// Whether an OPF identifier scheme (scheme-or-id, per
/// `scan_opf_identifiers`) denotes the Amazon ASIN. Mirrors
/// `epub::parser`'s recovery: EPUB-2 `opf:scheme="ASIN"` / `"MOBI-ASIN"`,
/// or the EPUB-3 `id="asin"` tag.
fn is_asin_identifier(scheme: &str) -> bool {
    scheme.eq_ignore_ascii_case("ASIN") || scheme.eq_ignore_ascii_case("MOBI-ASIN")
}

// ============================================================================
// Source-side
// ============================================================================

#[derive(Debug, Default)]
struct EpubMetadata {
    title: String,
    language: String,
    authors: Vec<String>,
    identifier: String,
    identifiers: Vec<(String, String)>,
    has_cover: bool,
    cover_path: Option<String>,
    ppd: Option<String>,
    date: Option<String>,
    primary_writing_mode: Option<String>,
    /// Number of spine XHTMLs whose `<html>` element carries `xml:lang`.
    html_lang_present: usize,
    /// Total number of spine XHTMLs scanned.
    html_doc_count: usize,
    /// All spine-doc `<img src>` / `<link href>` values that contain `..`
    /// segments. Each entry is `"<chapter-href>: <ref>"`. Format-only;
    /// the gate fires when this is non-empty.
    parent_escape_refs: Vec<String>,
    /// OPF `<package version>` string.
    opf_version: String,
    /// `<package version="3.X">` with no `<item properties="nav">` in the
    /// manifest. Required by EPUB 3; rejected by Apple Books when missing.
    epub3_missing_nav: bool,
}

fn extract_epub_metadata(epub_bytes: &[u8]) -> Result<EpubMetadata, String> {
    let cursor = Cursor::new(epub_bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|e| format!("not a valid zip: {}", e))?;

    let container_bytes = read_zip_entry(&mut archive, "META-INF/container.xml")
        .map_err(|e| format!("container.xml: {}", e))?;
    let opf_path = parse_container_xml(&container_bytes)
        .map_err(|e| format!("container.xml parse: {:?}", e))?;
    let opf_base = opf_path
        .rfind('/')
        .map(|i| &opf_path[..=i])
        .unwrap_or("")
        .to_string();
    let opf_bytes =
        read_zip_entry(&mut archive, &opf_path).map_err(|e| format!("opf {}: {}", opf_path, e))?;
    let enc = crate::util::extract_xml_encoding(&opf_bytes);
    let opf_str = crate::util::decode_text(&opf_bytes, enc);
    let opf = parse_opf(&opf_str).map_err(|e| format!("opf parse: {:?}", e))?;

    // The shared epub::parse_opf strips a lot of fields we now need for
    // round-trip; rescan the raw OPF source for `<dc:identifier>` schemes,
    // `<dc:date>`, and `<meta name="primary-writing-mode">`. The lossy
    // representation in `Metadata` is fine for the existing extractors but
    // can't tell us "0 identifiers" vs "1 unique" reliably across schemes.
    let identifiers = scan_opf_identifiers(&opf_str);
    let date = scan_opf_dc_date(&opf_str);
    let primary_writing_mode = scan_opf_primary_writing_mode(&opf_str);

    // Walk every spine XHTML once: count xml:lang on `<html>`, and scan
    // for gratuitous `..` parent escapes in `<img src>` / `<link href>`.
    let mut html_doc_count = 0usize;
    let mut html_lang_present = 0usize;
    let mut parent_escape_refs: Vec<String> = Vec::new();
    for spine_id in &opf.spine_ids {
        let Some((href, _)) = opf.manifest.get(spine_id) else {
            continue;
        };
        let full_path = format!("{}{}", opf_base, href);
        let chapter_dir = full_path.rfind('/').map(|i| &full_path[..i]).unwrap_or("");
        let Ok(xhtml_bytes) = read_zip_entry(&mut archive, &full_path) else {
            continue;
        };
        html_doc_count += 1;
        let enc = crate::util::extract_xml_encoding(&xhtml_bytes);
        let xhtml = crate::util::decode_text(&xhtml_bytes, enc);
        if html_root_has_xml_lang(&xhtml) {
            html_lang_present += 1;
        }
        for r in scan_parent_escape_refs(&xhtml) {
            if is_gratuitous_escape(&r, chapter_dir) {
                parent_escape_refs.push(format!("{}: {}", href, r));
            }
        }
    }

    // OPF package version + EPUB-3-nav check.
    let opf_version = scan_opf_version(&opf_str);
    let has_nav_item = scan_opf_has_nav_item(&opf_str);
    let epub3_missing_nav = opf_version.starts_with('3') && !has_nav_item;

    Ok(EpubMetadata {
        title: opf.metadata.title.clone(),
        language: opf.metadata.language.clone(),
        authors: opf.metadata.authors.clone(),
        identifier: opf.metadata.identifier.clone(),
        identifiers,
        has_cover: opf.metadata.cover_image.is_some(),
        cover_path: opf.metadata.cover_image.clone(),
        ppd: opf.metadata.page_progression_direction.clone(),
        date,
        primary_writing_mode,
        html_lang_present,
        html_doc_count,
        parent_escape_refs,
        opf_version,
        epub3_missing_nav,
    })
}

/// Resolve `ref` against `chapter_dir` (e.g. `"OEBPS"`) and check whether
/// the `..` segments are gratuitous: does the resolved absolute path land
/// back inside `chapter_dir`? When yes, the natural form is sibling-
/// relative and the `..` is just noise — Apple Books rejects this form
/// silently. When the `..` actually moves the target out of chapter_dir
/// (calibre's `../stylesheet.css` from a chapter in `OEBPS/`), the form is
/// legitimate.
fn is_gratuitous_escape(href: &str, chapter_dir: &str) -> bool {
    let resolved = resolve_relative(chapter_dir, href);
    let resolved_dir = resolved.rfind('/').map(|i| &resolved[..i]).unwrap_or("");
    resolved_dir == chapter_dir
}

/// Apply standard relative-URL resolution to produce an archive-root path.
fn resolve_relative(base_dir: &str, href: &str) -> String {
    let combined = if base_dir.is_empty() {
        href.to_string()
    } else {
        format!("{}/{}", base_dir, href)
    };
    let mut out: Vec<&str> = Vec::new();
    for seg in combined.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

/// Walk an XHTML document and return every `<img src>` or `<link href>`
/// value that contains a `..` path segment. The caller (extract_epub_metadata)
/// filters down to *gratuitous* escapes via `is_gratuitous_escape`.
fn scan_parent_escape_refs(xhtml: &str) -> Vec<String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    let mut out: Vec<String> = Vec::new();
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);
    let attr_for = |tag: &[u8]| -> Option<&'static [u8]> {
        match tag {
            b"img" | b"image" | b"audio" | b"video" | b"source" | b"script" | b"iframe" => {
                Some(b"src")
            }
            b"link" | b"a" => Some(b"href"),
            _ => None,
        }
    };
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let tag = e.local_name().as_ref().to_vec();
                if let Some(attr_name) = attr_for(&tag) {
                    for attr in e.attributes().flatten() {
                        if attr.key.local_name().as_ref() == attr_name {
                            let v = String::from_utf8_lossy(&attr.value);
                            if v.split('/').any(|seg| seg == "..") {
                                out.push(v.to_string());
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

/// Pull `<package version="..."/>` out of the OPF source.
fn scan_opf_version(opf_str: &str) -> String {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    let mut reader = Reader::from_str(opf_str);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if e.local_name().as_ref() == b"package" {
                    for attr in e.attributes().flatten() {
                        if attr.key.local_name().as_ref() == b"version" {
                            return String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                    return String::new();
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    String::new()
}

/// Does the OPF manifest contain at least one `<item properties="nav"/>`?
fn scan_opf_has_nav_item(opf_str: &str) -> bool {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    let mut reader = Reader::from_str(opf_str);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if e.local_name().as_ref() == b"item" {
                    for attr in e.attributes().flatten() {
                        if attr.key.local_name().as_ref() == b"properties"
                            && String::from_utf8_lossy(&attr.value)
                                .split_whitespace()
                                .any(|p| p == "nav")
                        {
                            return true;
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    false
}

/// Does the `<html>` root in this XHTML doc carry `xml:lang`?
fn html_root_has_xml_lang(xhtml: &str) -> bool {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if e.local_name().as_ref() == b"html" {
                    for attr in e.attributes().flatten() {
                        let key = attr.key.as_ref();
                        // Match `xml:lang` (with the `xml:` prefix) OR plain
                        // `lang` (HTML5 fallback some authors use).
                        if key == b"xml:lang" || key == b"lang" {
                            return true;
                        }
                    }
                    return false;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    false
}

/// Scan the raw OPF XML for every `<dc:identifier>` element and return
/// `(scheme, value)` pairs. The `scheme` comes from `opf:scheme` if present,
/// falling back to the `id` attribute — EPUB 3 forbids `opf:scheme`
/// (RSC-005), so EPUB-3-valid output tags identifiers by id (`id="asin"`),
/// the same convention `epub::parser` recovers from. Otherwise `""`.
/// (The shared `parse_opf` picks one identifier and loses schemes; we don't
/// want to widen that struct for a validator-only need.)
fn scan_opf_identifiers(opf_str: &str) -> Vec<(String, String)> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    fn scheme_of(e: &quick_xml::events::BytesStart) -> String {
        let mut scheme: Option<String> = None;
        let mut id: Option<String> = None;
        for attr in e.attributes().flatten() {
            match attr.key.local_name().as_ref() {
                b"scheme" => scheme = Some(String::from_utf8_lossy(&attr.value).to_string()),
                b"id" => id = Some(String::from_utf8_lossy(&attr.value).to_string()),
                _ => {}
            }
        }
        scheme.or(id).unwrap_or_default()
    }

    let mut reader = Reader::from_str(opf_str);
    reader.config_mut().trim_text(false);
    let mut out = Vec::new();
    let mut current_scheme: Option<String> = None;
    let mut in_identifier = false;
    let mut text_buf = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if e.local_name().as_ref() == b"identifier" {
                    in_identifier = true;
                    text_buf.clear();
                    current_scheme = Some(scheme_of(&e));
                }
            }
            Ok(Event::Empty(e)) => {
                if e.local_name().as_ref() == b"identifier" {
                    out.push((scheme_of(&e), String::new()));
                }
            }
            Ok(Event::Text(e)) if in_identifier => {
                text_buf.push_str(&String::from_utf8_lossy(e.as_ref()));
            }
            Ok(Event::End(e)) => {
                if e.local_name().as_ref() == b"identifier" {
                    out.push((
                        current_scheme.take().unwrap_or_default(),
                        text_buf.trim().to_string(),
                    ));
                    in_identifier = false;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

fn scan_opf_dc_date(opf_str: &str) -> Option<String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    let mut reader = Reader::from_str(opf_str);
    reader.config_mut().trim_text(false);
    let mut in_date = false;
    let mut buf = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"date" => {
                in_date = true;
                buf.clear();
            }
            Ok(Event::Text(e)) if in_date => {
                buf.push_str(&String::from_utf8_lossy(e.as_ref()));
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"date" => {
                let trimmed = buf.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
                in_date = false;
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    None
}

fn scan_opf_primary_writing_mode(opf_str: &str) -> Option<String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    let mut reader = Reader::from_str(opf_str);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event() {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.local_name().as_ref() == b"meta" => {
                let mut name: Option<String> = None;
                let mut content: Option<String> = None;
                for attr in e.attributes().flatten() {
                    match attr.key.local_name().as_ref() {
                        b"name" => name = Some(String::from_utf8_lossy(&attr.value).to_string()),
                        b"content" => {
                            content = Some(String::from_utf8_lossy(&attr.value).to_string())
                        }
                        _ => {}
                    }
                }
                if name.as_deref() == Some("primary-writing-mode") {
                    return content;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    None
}

fn read_zip_entry<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, std::io::Error> {
    use std::io::Read;
    let mut file = archive.by_name(name)?;
    let mut buf = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

// ============================================================================
// KFX-side
// ============================================================================

#[derive(Debug, Default)]
struct KfxMetadata {
    title: String,
    language: String,
    authors: Vec<String>,
    cover_image: Option<String>,
    book_id: Option<String>,
    ppd: Option<String>,
    asin: Option<String>,
    issue_date: Option<String>,
    is_vertical: bool,
    vertical_mode: Option<String>,
}

fn extract_kfx_metadata(kfx_bytes: &[u8]) -> Result<KfxMetadata, String> {
    let header = parse_container_header(kfx_bytes).map_err(|e| format!("kfx header: {:?}", e))?;
    if header.container_info_offset + header.container_info_length > kfx_bytes.len() {
        return Err("container info out of bounds".into());
    }
    let info_data = &kfx_bytes
        [header.container_info_offset..header.container_info_offset + header.container_info_length];
    let info =
        parse_container_info(info_data).map_err(|e| format!("kfx container info: {:?}", e))?;

    // Declared-base symbol table: doc-local ids start at the container's
    // declared import max_id, not at our static table's length (see
    // kfx::container::SymbolTable).
    let symbols = match info.doc_symbols {
        Some((off, len)) if off + len <= kfx_bytes.len() => {
            SymbolTable::from_fragment(Some(&kfx_bytes[off..off + len]))
        }
        _ => SymbolTable::from_fragment(None),
    };

    let resolve_sym = |id: u64| -> String { symbols.resolve(id).to_string() };

    let Some((idx_off, idx_len)) = info.index else {
        return Err("kfx: no index table".into());
    };
    let entities = parse_index_table(&kfx_bytes[idx_off..idx_off + idx_len], header.header_len);

    let metadata_type = KfxSymbol::Metadata as u32;
    let book_metadata_type = KfxSymbol::BookMetadata as u32;

    let mut out = KfxMetadata::default();
    // Ordered (key, value) pairs preserve repeated `author` entries in source
    // order — the HashMap-based collector above silently dropped all but the
    // last when multiple authors were declared.
    let mut kvs: Vec<(String, String)> = Vec::new();

    for ent in &entities {
        if ent.type_id == metadata_type {
            if let Some(value) = parse_entity(kfx_bytes, ent) {
                extract_ppd(&value, &resolve_sym, &mut out.ppd);
            }
        } else if ent.type_id == book_metadata_type
            && let Some(value) = parse_entity(kfx_bytes, ent)
        {
            extract_categorised(&value, &resolve_sym, &mut kvs);
        }
    }

    // Singleton fields take the first occurrence (matches calibre's "if not X"
    // guards in process_metadata_item).
    let first = |k: &str| kvs.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
    out.title = first("title").unwrap_or_default();
    out.language = first("language").unwrap_or_default();
    out.authors = kvs
        .iter()
        .filter(|(k, _)| k == "author")
        .map(|(_, v)| v.clone())
        .collect();
    out.cover_image = first("cover_image");
    out.book_id = first("book_id");
    out.asin = first("ASIN");
    out.issue_date = first("issue_date");

    // Detect any vertical writing mode anywhere in the KFX. Calibre's hint
    // logic (`epub_output.py:955`) emits `<meta name="primary-writing-mode">`
    // for any non-`horizontal-tb` book-level mode.
    let (is_vertical, vertical_mode) = detect_vertical_writing_mode(kfx_bytes, &resolve_sym)?;
    out.is_vertical = is_vertical;
    out.vertical_mode = vertical_mode;

    Ok(out)
}

/// Walk every entity and look for `writing_mode` fields. Returns
/// `(is_vertical, dominant_vertical_value)` — `is_vertical` is true iff any
/// `writing_mode` ends in `_rl` or `_lr`, and the dominant value picks
/// `vertical-rl` over `vertical-lr` when both exist (matches calibre's
/// most-cited-non-default selection).
fn detect_vertical_writing_mode<F>(
    kfx_bytes: &[u8],
    resolve_sym: &F,
) -> Result<(bool, Option<String>), String>
where
    F: Fn(u64) -> String,
{
    let header = parse_container_header(kfx_bytes).map_err(|e| format!("kfx header: {:?}", e))?;
    let info_data = &kfx_bytes
        [header.container_info_offset..header.container_info_offset + header.container_info_length];
    let info =
        parse_container_info(info_data).map_err(|e| format!("kfx container info: {:?}", e))?;
    let Some((idx_off, idx_len)) = info.index else {
        return Ok((false, None));
    };
    let entities = parse_index_table(&kfx_bytes[idx_off..idx_off + idx_len], header.header_len);

    let mut counts: HashMap<String, usize> = HashMap::new();
    for ent in &entities {
        if ent.offset + ent.length > kfx_bytes.len() {
            continue;
        }
        let entity = &kfx_bytes[ent.offset..ent.offset + ent.length];
        let ion = skip_enty_header(entity);
        let Ok(value) = IonParser::new(ion).parse() else {
            continue;
        };
        collect_writing_modes(&value, resolve_sym, &mut counts);
    }
    let mut best: Option<(String, usize)> = None;
    for (mode, n) in &counts {
        let css_value = mode.trim_start_matches('$').replace('_', "-");
        if css_value.ends_with("-rl") || css_value.ends_with("-lr") {
            match &best {
                Some((_, prev)) if prev >= n => {}
                _ => best = Some((css_value, *n)),
            }
        }
    }
    Ok((best.is_some(), best.map(|(m, _)| m)))
}

fn collect_writing_modes<F>(value: &IonValue, resolve_sym: &F, out: &mut HashMap<String, usize>)
where
    F: Fn(u64) -> String,
{
    let inner = match value {
        IonValue::Annotated(_, b) => b.as_ref(),
        v => v,
    };
    match inner {
        IonValue::Struct(fields) => {
            for (k, v) in fields {
                if resolve_sym(*k) == "writing_mode" {
                    let name = match v {
                        IonValue::Symbol(s) => resolve_sym(*s),
                        IonValue::String(s) => s.clone(),
                        _ => String::new(),
                    };
                    if !name.is_empty() {
                        *out.entry(name).or_insert(0) += 1;
                    }
                }
                collect_writing_modes(v, resolve_sym, out);
            }
        }
        IonValue::List(items) => {
            for item in items {
                collect_writing_modes(item, resolve_sym, out);
            }
        }
        _ => {}
    }
}

fn parse_entity(data: &[u8], ent: &crate::kfx::container::EntityLoc) -> Option<IonValue> {
    if ent.offset + ent.length > data.len() {
        return None;
    }
    let entity = &data[ent.offset..ent.offset + ent.length];
    let ion = skip_enty_header(entity);
    IonParser::new(ion).parse().ok()
}

/// Walk `metadata` ($258): `{reading_orders: [{page_progression_direction: $rtl, ...}, ...]}`.
fn extract_ppd<F>(value: &IonValue, resolve_sym: &F, ppd: &mut Option<String>)
where
    F: Fn(u64) -> String,
{
    let inner = match value {
        IonValue::Annotated(_, b) => b.as_ref(),
        v => v,
    };
    let IonValue::Struct(fields) = inner else {
        return;
    };
    for (k, v) in fields {
        if resolve_sym(*k) == "reading_orders"
            && let IonValue::List(items) = v
        {
            for r in items {
                if let IonValue::Struct(rfields) = r {
                    for (rk, rv) in rfields {
                        if resolve_sym(*rk) == "page_progression_direction"
                            && let IonValue::Symbol(s) = rv
                        {
                            *ppd = Some(resolve_sym(*s));
                        }
                    }
                }
            }
        }
    }
}

/// Walk `book_metadata` ($490): `{categorised_metadata: [{category, metadata: [{key, value}, ...]}, ...]}`.
/// Collect all (key, value) pairs into an ordered `Vec` so repeated keys
/// (like `author` for multi-author books) keep source order. Earlier versions
/// used a HashMap, which silently dropped all but the last value of any
/// repeated key.
fn extract_categorised<F>(value: &IonValue, resolve_sym: &F, out: &mut Vec<(String, String)>)
where
    F: Fn(u64) -> String,
{
    let inner = match value {
        IonValue::Annotated(_, b) => b.as_ref(),
        v => v,
    };
    let IonValue::Struct(fields) = inner else {
        return;
    };
    for (k, v) in fields {
        if resolve_sym(*k) == "categorised_metadata"
            && let IonValue::List(cats) = v
        {
            for cat in cats {
                let IonValue::Struct(cfields) = cat else {
                    continue;
                };
                for (ck, cv) in cfields {
                    if resolve_sym(*ck) == "metadata"
                        && let IonValue::List(entries) = cv
                    {
                        for entry in entries {
                            let IonValue::Struct(efields) = entry else {
                                continue;
                            };
                            let mut key: String = String::new();
                            let mut val: String = String::new();
                            for (ek, ev) in efields {
                                match resolve_sym(*ek).as_str() {
                                    "key" => {
                                        if let IonValue::String(s) = ev {
                                            key = s.clone();
                                        }
                                    }
                                    "value" => match ev {
                                        IonValue::String(s) => val = s.clone(),
                                        IonValue::Symbol(s) => val = resolve_sym(*s),
                                        IonValue::Bool(b) => val = b.to_string(),
                                        _ => {}
                                    },
                                    _ => {}
                                }
                            }
                            if !key.is_empty() {
                                out.push((key, val));
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_scheme_falls_back_to_id_attr() {
        // EPUB 2 carries the scheme as `opf:scheme`; EPUB-3-valid output
        // (where that attribute is illegal) tags identifiers by `id`.
        let opf = r#"<?xml version="1.0"?>
            <package xmlns:opf="http://www.idpf.org/2007/opf">
              <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
                <dc:identifier id="BookId">urn:uuid:1234</dc:identifier>
                <dc:identifier id="asin">B0CPJ2B88T</dc:identifier>
                <dc:identifier opf:scheme="MOBI-ASIN">B000000000</dc:identifier>
              </metadata>
            </package>"#;
        let ids = scan_opf_identifiers(opf);
        assert_eq!(
            ids,
            vec![
                ("BookId".to_string(), "urn:uuid:1234".to_string()),
                ("asin".to_string(), "B0CPJ2B88T".to_string()),
                ("MOBI-ASIN".to_string(), "B000000000".to_string()),
            ]
        );
        assert!(!is_asin_identifier(&ids[0].0));
        assert!(is_asin_identifier(&ids[1].0));
        assert!(is_asin_identifier(&ids[2].0));
    }
}
