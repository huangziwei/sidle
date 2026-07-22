//! EPUB-3 standalone validator. Catches format-level defects that would
//! either corrupt downstream KFX conversion or get rejected by strict
//! readers (Apple Books). Distinct from the pair-conversion validator at
//! [`crate::validate`], which compares semantic preservation across
//! formats; this one checks one EPUB against the EPUB-3 spec.
//!
//! Rules covered (focused on the failure modes we've actually hit):
//!
//! - `mimetype` is the first zip entry, `STORED`, exact 20-byte content
//!   `application/epub+zip` (EPUB 3.3 §3.4).
//! - `META-INF/container.xml` exists and points to an OPF that's in the zip.
//! - Every manifest `<item href>` resolves to a file inside the zip.
//! - Every zip entry under the OPF directory is declared in the manifest
//!   (besides `mimetype` and `META-INF/*`).
//! - Every spine `<itemref idref>` matches a manifest id.
//! - Exactly one manifest item carries `properties="nav"` (EPUB 3 nav doc).
//! - Every spine item with `linear="no"` is the target of an `<a href>`
//!   from another doc in the publication (EPUB 3.3 §5.8.2 reachability —
//!   the rule that fails downstream KFX conversion silently).
//! - Every internal reference in XHTML resolves to a file in the zip
//!   (epubcheck RSC-007): hyperlinks (`<a>`/`<area>`) and resource loads
//!   (`<img>`, `<link>`, SVG `<image>`/`<use>`, `<object>`, and the media
//!   elements). Any `#fragment` a reference carries resolves to an element
//!   `id` in the (local, XHTML) target document — same-document `#frag`
//!   included (epubcheck RSC-012). Fragments into SVG/other targets, `srcset`,
//!   and CSS `url()` are not yet indexed.
//! - No href in OPF or XHTML resolves to a path outside the OPF root
//!   directory. `..` parent segments inside the OPF tree are fine (e.g.
//!   `../style.css` from a chapter is legal); escapes above the OPF root
//!   are not — Apple Books rejects them silently.
//! - Every content document declares `<!DOCTYPE html>` (not a legacy XHTML DTD)
//!   and carries a non-empty `<title>` — the two content-conformance rules
//!   (epubcheck HTM-004 / RSC-005) real books trip most often.
//!
//! Each rule is keyed to its **epubcheck message id** ([`Rule::message_id`] —
//! `RSC-007`, `HTM-004`, …), drawn from the full [`messages::CATALOG`] (every
//! id epubcheck defines, with its default severity), so a [`Finding`] is
//! directly comparable with W3C epubcheck output. Three checks are bokai-native
//! (kebab ids) where epubcheck has no dedicated code. Port coverage is tracked
//! by [`epubcheck_error_coverage`].
//!
//! Out of scope (deferred): full XSD/RNG/Schematron validation, CSS
//! validation, content document well-formedness, and fragments into SVG
//! targets. Shell out to W3C's `epubcheck` Java tool for those.

pub mod messages;
pub mod opf;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{self, Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

#[derive(Debug, Default)]
pub struct Report {
    pub violations: Vec<Violation>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }

    pub fn push(&mut self, v: Violation) {
        self.violations.push(v);
    }

    pub fn has_rule(&self, rule: Rule) -> bool {
        self.violations.iter().any(|v| v.rule == rule)
    }

    /// Lower these EPUB violations into the unified
    /// [`Finding`](crate::validate::Finding) model the book
    /// editor consumes (via [`crate::validate::source::validate`]). The rich
    /// [`Violation`]/[`Rule`] internals stay; this is just the projection.
    pub fn into_findings(self) -> Vec<crate::validate::Finding> {
        self.violations
            .into_iter()
            .map(Violation::into_finding)
            .collect()
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.violations.is_empty() {
            return write!(f, "epub3 validate: clean (0 violations)");
        }
        writeln!(f, "epub3 validate: {} violation(s)", self.violations.len())?;
        for v in &self.violations {
            writeln!(
                f,
                "  [{}] {}: {}",
                v.rule.message_id(),
                v.location,
                v.message
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Violation {
    pub rule: Rule,
    pub location: String,
    pub message: String,
}

impl Violation {
    fn new(rule: Rule, location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            rule,
            location: location.into(),
            message: message.into(),
        }
    }

    fn into_finding(self) -> crate::validate::Finding {
        crate::validate::Finding {
            check: "epub",
            rule: self.rule.message_id().to_string(),
            severity: self.rule.severity(),
            location: self.location,
            message: self.message,
            fix: self.rule.fix_hint(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    ZipMalformed,
    MimetypeNotFirst,
    MimetypeNotStored,
    MimetypeBadContent,
    MissingContainerXml,
    OpfMissing,
    OpfParseError,
    ManifestFileMissing,
    FileNotInManifest,
    SpineIdrefUnknown,
    NavMissing,
    NavDuplicated,
    NonLinearUnreachable,
    BrokenHref,
    FragmentNotDefined,
    HrefEscapesOpfRoot,
    IrregularDoctype,
    EmptyTitle,
    // OPF package/manifest/spine structure (epubcheck OPF-*).
    SpineNoLinear,
    SpineDuplicateIdref,
    DuplicateManifestResource,
    ManifestListsPackageDoc,
    ItemHrefHasFragment,
    UniqueIdentifierMissing,
    UniqueIdentifierNotFound,
    SpineTocNotNcx,
    FallbackNotFound,
    GuideReferenceNotInManifest,
    DuplicateZipEntry,
    // OCF container / package integrity (epubcheck PKG-* / RSC-*).
    MimetypeExtraField,
    FilenameForbiddenChar,
    FilenameTrailingDot,
    ResourceInMetaInf,
    FileUrlNotAllowed,
    // XML content conformance (epubcheck HTM-* / RSC-*).
    XmlVersionNot10,
    ExternalEntity,
    StylesheetFragment,
    NoOpsRootfile,
    RelativeUrlWithQuery,
}

impl Rule {
    /// Every variant, so callers (coverage reporting, the mapping-invariant
    /// tests) can iterate the rule set. `message_id`'s match is
    /// compiler-exhaustive; keep this in sync (a test asserts the length).
    pub const ALL: &'static [Rule] = &[
        Rule::ZipMalformed,
        Rule::MimetypeNotFirst,
        Rule::MimetypeNotStored,
        Rule::MimetypeBadContent,
        Rule::MissingContainerXml,
        Rule::OpfMissing,
        Rule::OpfParseError,
        Rule::ManifestFileMissing,
        Rule::FileNotInManifest,
        Rule::SpineIdrefUnknown,
        Rule::NavMissing,
        Rule::NavDuplicated,
        Rule::NonLinearUnreachable,
        Rule::BrokenHref,
        Rule::FragmentNotDefined,
        Rule::HrefEscapesOpfRoot,
        Rule::IrregularDoctype,
        Rule::EmptyTitle,
        Rule::SpineNoLinear,
        Rule::SpineDuplicateIdref,
        Rule::DuplicateManifestResource,
        Rule::ManifestListsPackageDoc,
        Rule::ItemHrefHasFragment,
        Rule::UniqueIdentifierMissing,
        Rule::UniqueIdentifierNotFound,
        Rule::SpineTocNotNcx,
        Rule::FallbackNotFound,
        Rule::GuideReferenceNotInManifest,
        Rule::DuplicateZipEntry,
        Rule::MimetypeExtraField,
        Rule::FilenameForbiddenChar,
        Rule::FilenameTrailingDot,
        Rule::ResourceInMetaInf,
        Rule::FileUrlNotAllowed,
        Rule::XmlVersionNot10,
        Rule::ExternalEntity,
        Rule::StylesheetFragment,
        Rule::NoOpsRootfile,
        Rule::RelativeUrlWithQuery,
    ];

    /// This rule's epubcheck message id (e.g. `"RSC-007"`) — the identity a
    /// [`crate::validate::Finding`] carries, so bokai's output is directly
    /// comparable with W3C epubcheck. Ids in `XXX-NNN` form are real epubcheck
    /// messages present in [`messages::CATALOG`] (enforced by a test); the two
    /// kebab ids are bokai-native — epubcheck enforces the single-nav-document
    /// requirement through its OPF schema (RSC-005 channel) rather than a
    /// dedicated code.
    pub fn message_id(self) -> &'static str {
        match self {
            Rule::ZipMalformed => "PKG-004",
            Rule::MimetypeNotFirst => "PKG-006",
            Rule::MimetypeNotStored => "PKG-007",
            Rule::MimetypeBadContent => "PKG-007",
            Rule::MissingContainerXml => "RSC-002",
            Rule::OpfMissing => "PKG-020",
            Rule::OpfParseError => "RSC-005",
            Rule::ManifestFileMissing => "RSC-001",
            Rule::FileNotInManifest => "OPF-003",
            Rule::SpineIdrefUnknown => "OPF-049",
            Rule::NavMissing => "nav-missing",
            Rule::NavDuplicated => "nav-duplicated",
            Rule::NonLinearUnreachable => "OPF-096",
            Rule::BrokenHref => "RSC-007",
            Rule::FragmentNotDefined => "RSC-012",
            Rule::HrefEscapesOpfRoot => "RSC-026",
            Rule::IrregularDoctype => "HTM-004",
            Rule::EmptyTitle => "RSC-005",
            Rule::SpineNoLinear => "OPF-033",
            Rule::SpineDuplicateIdref => "OPF-034",
            Rule::DuplicateManifestResource => "OPF-074",
            Rule::ManifestListsPackageDoc => "OPF-099",
            Rule::ItemHrefHasFragment => "OPF-091",
            Rule::UniqueIdentifierMissing => "OPF-048",
            Rule::UniqueIdentifierNotFound => "OPF-030",
            Rule::SpineTocNotNcx => "OPF-050",
            Rule::FallbackNotFound => "OPF-040",
            Rule::GuideReferenceNotInManifest => "OPF-031",
            Rule::DuplicateZipEntry => "OPF-060",
            Rule::MimetypeExtraField => "PKG-005",
            Rule::FilenameForbiddenChar => "PKG-009",
            Rule::FilenameTrailingDot => "PKG-011",
            Rule::ResourceInMetaInf => "PKG-025",
            Rule::FileUrlNotAllowed => "RSC-030",
            Rule::XmlVersionNot10 => "HTM-001",
            Rule::ExternalEntity => "HTM-003",
            Rule::StylesheetFragment => "RSC-013",
            Rule::NoOpsRootfile => "RSC-003",
            Rule::RelativeUrlWithQuery => "RSC-033",
        }
    }

    /// Severity of this rule in the unified [`crate::validate::Severity`] model.
    /// Undeclared extra resources are real but widely tolerated (Warning);
    /// every other rule either violates a MUST or corrupts conversion (Error).
    fn severity(self) -> crate::validate::Severity {
        use crate::validate::Severity;
        match self {
            Rule::FileNotInManifest => Severity::Warning,
            _ => Severity::Error,
        }
    }

    /// A structured repair proposal for the rules whose fix is unambiguous.
    /// Others carry no hint yet (the editor still shows the message).
    fn fix_hint(self) -> Option<crate::validate::FixHint> {
        use crate::validate::FixHint;
        match self {
            Rule::NavMissing => Some(FixHint::new(
                "add-nav-doc",
                "add an EPUB 3 nav document (properties=\"nav\") listing the book's chapters",
            )),
            Rule::NonLinearUnreachable => Some(FixHint::new(
                "make-linear-or-link",
                "make this spine item linear, or add an <a href> to it from another document",
            )),
            Rule::IrregularDoctype => Some(FixHint::new(
                "use-html5-doctype",
                "replace the document type declaration with `<!DOCTYPE html>`",
            )),
            Rule::EmptyTitle => Some(FixHint::new(
                "fill-title",
                "give the content document's <title> element non-empty text",
            )),
            _ => None,
        }
    }
}

/// Port progress against the epubcheck catalog: `(implemented, total)`
/// error-level messages. `implemented` is the count of distinct epubcheck
/// error-level ids some [`Rule`] emits; `total` is [`messages::error_level_count`].
/// Bokai-native rule ids (kebab) match no catalog id, so they don't inflate it.
pub fn epubcheck_error_coverage() -> (usize, usize) {
    let implemented: HashSet<&str> = Rule::ALL.iter().map(|r| r.message_id()).collect();
    let covered = messages::CATALOG
        .iter()
        .filter(|m| m.severity == Some(crate::validate::Severity::Error))
        .filter(|m| implemented.contains(m.id))
        .count();
    (covered, messages::error_level_count())
}

/// Validate `epub_bytes` against the rules listed in the module docs.
/// Returns a [`Report`]; `report.is_clean()` is true iff no rule fired.
pub fn validate(epub_bytes: &[u8]) -> Report {
    let mut report = Report::default();

    check_mimetype_header(epub_bytes, &mut report);

    let cursor = Cursor::new(epub_bytes);
    let mut zip = match ZipArchive::new(cursor) {
        Ok(z) => z,
        Err(e) => {
            report.push(Violation::new(
                Rule::ZipMalformed,
                "<archive>",
                format!("{e}"),
            ));
            return report;
        }
    };

    let opf_path = match read_container_opf_path(&mut zip, &mut report) {
        Ok(p) => p,
        Err(v) => {
            report.push(v);
            return report;
        }
    };

    let opf_text = match read_text(&mut zip, &opf_path) {
        Ok(t) => t,
        Err(_) => {
            report.push(Violation::new(
                Rule::OpfMissing,
                opf_path,
                "OPF path referenced by container.xml is not present in the zip",
            ));
            return report;
        }
    };

    let pkg = match opf::parse(&opf_text) {
        Ok(p) => p,
        Err(e) => {
            report.push(Violation::new(
                Rule::OpfParseError,
                opf_path,
                format!("{e}"),
            ));
            return report;
        }
    };

    let zip_names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .collect();
    check_duplicate_zip_entries(&zip_names, &mut report);
    check_ocf_filenames(&zip_names, &mut report);
    let zip_paths: HashSet<String> = zip_names.into_iter().collect();
    let opf_dir = opf_path
        .rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default();

    check_manifest_files(&pkg, &opf_dir, &zip_paths, &opf_path, &mut report);
    check_files_in_manifest(&pkg, &opf_dir, &zip_paths, &opf_path, &mut report);
    check_spine_idrefs(&pkg, &opf_path, &mut report);
    check_nav_present(&pkg, &opf_path, &mut report);
    check_parent_paths_in_opf(&pkg, &opf_dir, &opf_path, &mut report);
    check_opf_structure(&pkg, &opf_dir, &opf_path, &mut report);
    check_xhtml_hrefs_and_reachability(
        &pkg,
        &opf_dir,
        &mut zip,
        &zip_paths,
        &opf_path,
        &mut report,
    );
    check_content_conformance(&pkg, &opf_dir, &mut zip, &mut report);
    check_xml_conformance(&opf_text, &opf_path, &mut report);
    check_xml_resources(&pkg, &opf_dir, &mut zip, &mut report);

    report
}

/// HTM-001 (XML version must be 1.0) and HTM-003 (external entity declarations
/// are forbidden in EPUB 3) over every XML-based publication resource.
fn check_xml_resources(
    pkg: &opf::Package,
    opf_dir: &str,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    report: &mut Report,
) {
    for item in &pkg.manifest {
        if !is_xml_based(&item.media_type) {
            continue;
        }
        let path = join_opf(opf_dir, &item.href);
        let Ok(text) = read_text(zip, &path) else {
            continue;
        };
        check_xml_conformance(&text, &path, report);
    }
}

/// HTM-001 / HTM-003 for one XML document's text.
fn check_xml_conformance(text: &str, path: &str, report: &mut Report) {
    if let Some(ver) = xml_declaration_version(text)
        && ver != "1.0"
    {
        report.push(Violation::new(
            Rule::XmlVersionNot10,
            path.to_string(),
            format!("XML declaration version {ver:?}; EPUB requires XML 1.0"),
        ));
    }
    if has_external_entity(text) {
        report.push(Violation::new(
            Rule::ExternalEntity,
            path.to_string(),
            "external entity declaration is not allowed in EPUB 3 documents",
        ));
    }
}

/// `version="…"` from the leading `<?xml …?>` declaration, if any. Find-based
/// (never slices at a non-char-boundary).
fn xml_declaration_version(s: &str) -> Option<&str> {
    let start = s.find("<?xml")?;
    let end = start + s[start..].find("?>")?;
    let decl = &s[start..end];
    let after = &decl[decl.find("version")?..];
    let q = after.find(['"', '\''])?;
    let quote = &after[q..q + 1];
    let val_start = q + 1;
    let val_end = val_start + after[val_start..].find(quote)?;
    Some(&after[val_start..val_end])
}

/// True if the document declares an external entity (`<!ENTITY … SYSTEM/PUBLIC
/// …>`). Scans each `<!ENTITY` declaration up to its `>`; UTF-8 safe.
fn has_external_entity(s: &str) -> bool {
    let mut from = 0;
    while let Some(rel) = s[from..].find("<!ENTITY") {
        let start = from + rel;
        let end = s[start..].find('>').map(|e| start + e).unwrap_or(s.len());
        let decl = &s[start..end];
        if decl.contains("SYSTEM") || decl.contains("PUBLIC") {
            return true;
        }
        from = (start + "<!ENTITY".len()).min(s.len());
    }
    false
}

/// XML-based media type: `application/xml`, `text/xml`, or any `…+xml`.
fn is_xml_based(media_type: &str) -> bool {
    let mt = media_type.trim();
    mt.eq_ignore_ascii_case("application/xml")
        || mt.eq_ignore_ascii_case("text/xml")
        || mt
            .rsplit_once('+')
            .is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case("xml"))
}

/// OPF package/manifest/spine structural rules that need no zip access beyond
/// the parsed [`opf::Package`] (epubcheck OPF-030/031/033/034/040/048/050/074/
/// 091/096/099). Resource *existence* is covered separately by
/// [`check_manifest_files`]; these check the package's internal consistency.
fn check_opf_structure(pkg: &opf::Package, opf_dir: &str, opf_path: &str, report: &mut Report) {
    let manifest_ids: HashSet<&str> = pkg.manifest.iter().map(|m| m.id.as_str()).collect();

    // OPF-048 / OPF-030: the package must declare a unique-identifier that
    // resolves to a <dc:identifier id="…">.
    match pkg.unique_identifier.as_deref() {
        None | Some("") => report.push(Violation::new(
            Rule::UniqueIdentifierMissing,
            opf_path,
            "package element is missing its required unique-identifier attribute",
        )),
        Some(uid) if !pkg.identifier_ids.iter().any(|id| id == uid) => {
            report.push(Violation::new(
                Rule::UniqueIdentifierNotFound,
                opf_path,
                format!("unique-identifier {uid:?} matches no <dc:identifier id=…>"),
            ));
        }
        _ => {}
    }

    // OPF-033: at least one spine item must be linear.
    if !pkg.spine.is_empty() && pkg.spine.iter().all(|s| s.linear == Some(false)) {
        report.push(Violation::new(
            Rule::SpineNoLinear,
            opf_path,
            "the spine contains no linear resources (every itemref is linear=\"no\")",
        ));
    }

    // OPF-034: no manifest id may be referenced more than once by the spine.
    let mut seen_idref: HashSet<&str> = HashSet::new();
    for s in &pkg.spine {
        if !seen_idref.insert(s.idref.as_str()) {
            report.push(Violation::new(
                Rule::SpineDuplicateIdref,
                opf_path,
                format!("spine references manifest id {:?} more than once", s.idref),
            ));
        }
    }

    // OPF-074 / OPF-099 / OPF-091: manifest resource hygiene.
    let mut seen_href: HashSet<String> = HashSet::new();
    for item in &pkg.manifest {
        if item.href.contains('#') {
            report.push(Violation::new(
                Rule::ItemHrefHasFragment,
                opf_path,
                format!(
                    "manifest item href {:?} must not carry a fragment identifier",
                    item.href
                ),
            ));
        }
        let resolved = join_opf(opf_dir, &item.href);
        if resolved == opf_path {
            report.push(Violation::new(
                Rule::ManifestListsPackageDoc,
                opf_path,
                "the manifest must not list the package document itself",
            ));
        }
        if resolved.starts_with("META-INF/") {
            report.push(Violation::new(
                Rule::ResourceInMetaInf,
                opf_path,
                format!("publication resource {resolved:?} must not be located in META-INF/"),
            ));
        }
        if !seen_href.insert(resolved.clone()) {
            report.push(Violation::new(
                Rule::DuplicateManifestResource,
                opf_path,
                format!("resource {resolved:?} is declared by more than one manifest item"),
            ));
        }
    }

    // OPF-040: every fallback attribute must point at an existing manifest id.
    for item in &pkg.manifest {
        if let Some(fb) = &item.fallback
            && !manifest_ids.contains(fb.as_str())
        {
            report.push(Violation::new(
                Rule::FallbackNotFound,
                opf_path,
                format!(
                    "manifest item id={:?} has fallback={fb:?}, which is not a manifest id",
                    item.id
                ),
            ));
        }
    }

    // OPF-050: the spine `toc` attribute must reference an NCX manifest item.
    if let Some(toc_id) = &pkg.spine_toc
        && let Some(item) = pkg.manifest_by_id(toc_id)
        && !item
            .media_type
            .eq_ignore_ascii_case("application/x-dtbncx+xml")
    {
        report.push(Violation::new(
            Rule::SpineTocNotNcx,
            opf_path,
            format!(
                "spine toc={toc_id:?} points at media-type {:?}; NCX (application/x-dtbncx+xml) is required",
                item.media_type
            ),
        ));
    }

    // OPF-031: every <guide><reference href> must be a declared manifest item.
    let manifest_hrefs: HashSet<String> = pkg
        .manifest
        .iter()
        .map(|m| join_opf(opf_dir, &m.href))
        .collect();
    for href in &pkg.guide_hrefs {
        let file = href.split('#').next().unwrap_or(href);
        if file.is_empty() || file.contains("://") {
            continue;
        }
        let resolved = join_opf(opf_dir, file);
        if !manifest_hrefs.contains(&resolved) {
            report.push(Violation::new(
                Rule::GuideReferenceNotInManifest,
                opf_path,
                format!("guide reference {href:?} is not declared in the manifest"),
            ));
        }
    }
}

/// OPF-060: zip entry names must be unique (after the fact that a reader keys
/// resources by name). A duplicate name shadows a resource unpredictably.
fn check_duplicate_zip_entries(zip_names: &[String], report: &mut Report) {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut reported: HashSet<&str> = HashSet::new();
    for name in zip_names {
        if name.ends_with('/') {
            continue; // directory entries
        }
        if !seen.insert(name.as_str()) && reported.insert(name.as_str()) {
            report.push(Violation::new(
                Rule::DuplicateZipEntry,
                name.clone(),
                format!("duplicate entry {name:?} in the container (names must be unique)"),
            ));
        }
    }
}

/// OCF file-name constraints (EPUB OCF §4.2). PKG-009: a name must not contain
/// the OCF-restricted characters `" * : < > ? \ |` or any control character
/// (C0/C1/DEL). PKG-011: no path segment may end with `.`. `/` is the path
/// separator and is allowed.
fn check_ocf_filenames(zip_names: &[String], report: &mut Report) {
    const FORBIDDEN: &[char] = &['"', '*', ':', '<', '>', '?', '\\', '|'];
    for name in zip_names {
        if let Some(bad) = name
            .chars()
            .find(|c| FORBIDDEN.contains(c) || c.is_control() || ('\u{80}'..='\u{9F}').contains(c))
        {
            report.push(Violation::new(
                Rule::FilenameForbiddenChar,
                name.clone(),
                format!("file name {name:?} contains the OCF-disallowed character {bad:?}"),
            ));
        }
        if name.split('/').any(|seg| seg.ends_with('.')) {
            report.push(Violation::new(
                Rule::FilenameTrailingDot,
                name.clone(),
                format!("file name {name:?} has a path segment ending with '.'"),
            ));
        }
    }
}

// =========================================================================
// Content-document conformance: EPUB 3 content docs need `<!DOCTYPE html>`
// and a non-empty <title> (epubcheck HTM-004 / RSC-005). Pre-EPUB-3 source
// (a Sigil-authored book carried through AZW3: XHTML 1.1 DOCTYPE, empty
// <title>) tripped both — passthrough export emitted it verbatim and neither
// this validator nor the pair harness noticed.
// =========================================================================

fn check_content_conformance(
    pkg: &opf::Package,
    opf_dir: &str,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    report: &mut Report,
) {
    for item in &pkg.manifest {
        if !is_xhtml(&item.media_type) {
            continue;
        }
        let path = join_opf(opf_dir, &item.href);
        let Ok(text) = read_text(zip, &path) else {
            continue;
        };
        if let Some(dt) = find_doctype(&text)
            && !dt.eq_ignore_ascii_case("<!DOCTYPE html>")
        {
            report.push(Violation::new(
                Rule::IrregularDoctype,
                path.clone(),
                format!("irregular DOCTYPE {dt:?}; EPUB 3 requires `<!DOCTYPE html>`"),
            ));
        }
        if has_empty_title(&text) {
            report.push(Violation::new(
                Rule::EmptyTitle,
                path,
                "content document has an empty <title>; EPUB 3 requires non-empty title text",
            ));
        }
    }
}

/// The document's `<!DOCTYPE …>` declaration text, matched at the two casings
/// real content uses. Uses byte-boundary-safe `find` (never slices at an
/// arbitrary offset that could split a multi-byte char).
fn find_doctype(s: &str) -> Option<&str> {
    let start = s.find("<!DOCTYPE").or_else(|| s.find("<!doctype"))?;
    let end = start + s[start..].find('>')? + 1;
    Some(&s[start..end])
}

/// True when the first `<title>` element is empty or self-closing.
fn has_empty_title(s: &str) -> bool {
    let Some(open) = s.find("<title") else {
        return false;
    };
    let Some(rel) = s[open..].find('>') else {
        return false;
    };
    let gt = open + rel;
    if s.as_bytes()[gt - 1] == b'/' {
        return true; // <title/>
    }
    match s[gt + 1..].find("</title>") {
        Some(r) => s[gt + 1..gt + 1 + r].trim().is_empty(),
        None => false,
    }
}

// =========================================================================
// Rule: mimetype is first entry, STORED, exact "application/epub+zip"
// =========================================================================

fn check_mimetype_header(bytes: &[u8], report: &mut Report) {
    // Local file header layout: PK\x03\x04 (4) + version (2) + flags (2) +
    // compression (2) + time/date (4) + crc (4) + comp size (4) + uncomp
    // size (4) + name len (2) + extra len (2) = 30 bytes, then name, extra,
    // data. EPUB 3.3 §3.4 requires the first entry to be `mimetype` with
    // compression method 0 (STORED), no extra field, content equal to the
    // 20 bytes `application/epub+zip`.
    const REQUIRED: &[u8] = b"application/epub+zip";

    if bytes.len() < 30 + 8 + REQUIRED.len() {
        report.push(Violation::new(
            Rule::MimetypeNotFirst,
            "<archive>",
            "file too small to contain a valid mimetype entry",
        ));
        return;
    }
    if &bytes[0..4] != b"PK\x03\x04" {
        report.push(Violation::new(
            Rule::MimetypeNotFirst,
            "<archive>",
            "missing zip local-file-header signature at offset 0",
        ));
        return;
    }
    let compression = u16::from_le_bytes([bytes[8], bytes[9]]);
    let name_len = u16::from_le_bytes([bytes[26], bytes[27]]) as usize;
    let extra_len = u16::from_le_bytes([bytes[28], bytes[29]]) as usize;

    if bytes.len() < 30 + name_len + extra_len + REQUIRED.len() {
        report.push(Violation::new(
            Rule::MimetypeNotFirst,
            "<archive>",
            "first zip entry header is truncated",
        ));
        return;
    }
    let name = &bytes[30..30 + name_len];
    if name != b"mimetype" {
        report.push(Violation::new(
            Rule::MimetypeNotFirst,
            "<archive>",
            format!(
                "first zip entry must be `mimetype`, found {:?}",
                String::from_utf8_lossy(name)
            ),
        ));
        return;
    }
    if compression != 0 {
        report.push(Violation::new(
            Rule::MimetypeNotStored,
            "mimetype",
            format!("compression method must be 0 (STORED), found {compression}"),
        ));
    }
    if extra_len != 0 {
        report.push(Violation::new(
            Rule::MimetypeExtraField,
            "mimetype",
            format!("mimetype entry has a {extra_len}-byte extra field; none is permitted"),
        ));
    }
    let content_start = 30 + name_len + extra_len;
    let content = &bytes[content_start..content_start + REQUIRED.len()];
    if content != REQUIRED {
        report.push(Violation::new(
            Rule::MimetypeBadContent,
            "mimetype",
            format!(
                "expected {:?}, found {:?}",
                String::from_utf8_lossy(REQUIRED),
                String::from_utf8_lossy(content),
            ),
        ));
    }
}

// =========================================================================
// Container.xml → OPF path
// =========================================================================

fn read_container_opf_path(
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    report: &mut Report,
) -> Result<String, Violation> {
    let text = read_text(zip, "META-INF/container.xml").map_err(|_| {
        Violation::new(
            Rule::MissingContainerXml,
            "META-INF/container.xml",
            "container.xml is missing from the zip",
        )
    })?;
    // Collect every <rootfile>: (full-path, media-type).
    let mut rootfiles: Vec<(String, String)> = Vec::new();
    let mut reader = Reader::from_str(&text);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.name().as_ref() == b"rootfile" => {
                let (mut full_path, mut media_type) = (None, String::new());
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"full-path" => {
                            full_path = Some(String::from_utf8_lossy(&attr.value).to_string())
                        }
                        b"media-type" => {
                            media_type = String::from_utf8_lossy(&attr.value).to_string()
                        }
                        _ => {}
                    }
                }
                if let Some(fp) = full_path {
                    rootfiles.push((fp, media_type));
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(Violation::new(
                    Rule::MissingContainerXml,
                    "META-INF/container.xml",
                    format!("parse error: {e}"),
                ));
            }
            _ => {}
        }
    }

    // RSC-003: a rootfile with the OPS package media-type is required.
    const OPS: &str = "application/oebps-package+xml";
    if let Some((fp, _)) = rootfiles
        .iter()
        .find(|(_, mt)| mt.eq_ignore_ascii_case(OPS))
    {
        return Ok(fp.clone());
    }
    if let Some((fp, _)) = rootfiles.first() {
        // A rootfile exists but none declares the OPS media-type: flag RSC-003
        // and proceed with the first so the remaining checks still run.
        report.push(Violation::new(
            Rule::NoOpsRootfile,
            "META-INF/container.xml",
            format!("no <rootfile> declares media-type {OPS:?}"),
        ));
        return Ok(fp.clone());
    }
    Err(Violation::new(
        Rule::NoOpsRootfile,
        "META-INF/container.xml",
        "container.xml has no <rootfile full-path=…> element",
    ))
}

// =========================================================================
// Manifest ↔ files
// =========================================================================

fn check_manifest_files(
    pkg: &opf::Package,
    opf_dir: &str,
    zip_paths: &HashSet<String>,
    opf_path: &str,
    report: &mut Report,
) {
    for item in &pkg.manifest {
        let resolved = join_opf(opf_dir, &item.href);
        if !zip_paths.contains(&resolved) {
            report.push(Violation::new(
                Rule::ManifestFileMissing,
                opf_path,
                format!(
                    "manifest item id={:?} href={:?} -> {:?} is not present in the zip",
                    item.id, item.href, resolved
                ),
            ));
        }
    }
}

fn check_files_in_manifest(
    pkg: &opf::Package,
    opf_dir: &str,
    zip_paths: &HashSet<String>,
    opf_path: &str,
    report: &mut Report,
) {
    let manifest_paths: HashSet<String> = pkg
        .manifest
        .iter()
        .map(|m| join_opf(opf_dir, &m.href))
        .collect();
    for path in zip_paths {
        if path == "mimetype" || path.starts_with("META-INF/") || path == opf_path {
            continue;
        }
        if path.ends_with('/') {
            continue;
        }
        if manifest_paths.contains(path) {
            continue;
        }
        report.push(Violation::new(
            Rule::FileNotInManifest,
            opf_path,
            format!("zip entry {:?} is not declared in the manifest", path),
        ));
    }
}

fn check_spine_idrefs(pkg: &opf::Package, opf_path: &str, report: &mut Report) {
    let ids: HashSet<&str> = pkg.manifest.iter().map(|m| m.id.as_str()).collect();
    for s in &pkg.spine {
        if !ids.contains(s.idref.as_str()) {
            report.push(Violation::new(
                Rule::SpineIdrefUnknown,
                opf_path,
                format!("spine idref={:?} does not match any manifest id", s.idref),
            ));
        }
    }
}

// =========================================================================
// Nav doc
// =========================================================================

fn check_nav_present(pkg: &opf::Package, opf_path: &str, report: &mut Report) {
    let nav_items: Vec<&opf::ManifestItem> = pkg
        .manifest
        .iter()
        .filter(|m| m.properties.iter().any(|p| p == "nav"))
        .collect();
    match nav_items.len() {
        0 => report.push(Violation::new(
            Rule::NavMissing,
            opf_path,
            "no manifest item has properties=\"nav\" (required by EPUB 3)",
        )),
        1 => {}
        n => report.push(Violation::new(
            Rule::NavDuplicated,
            opf_path,
            format!("{n} manifest items have properties=\"nav\"; exactly one is allowed"),
        )),
    }
}

// =========================================================================
// XHTML href scan: reachability for non-linear spine + broken href + parent paths
// =========================================================================

fn check_xhtml_hrefs_and_reachability(
    pkg: &opf::Package,
    opf_dir: &str,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    zip_paths: &HashSet<String>,
    opf_path: &str,
    report: &mut Report,
) {
    // Collect every internal target referenced from any XHTML in the manifest:
    // hyperlinks (`<a>`/`<area>`) plus resource references (`<img>`, `<link>`,
    // SVG `<image>`/`<use>`, `<object>`, media elements, …). Paths are resolved
    // relative to the XHTML they appear in. Only hyperlink targets feed the
    // reachability check; every reference feeds RSC-007 (present in the zip) and
    // the OPF-root-escape check.
    let mut hyperlink_targets: HashSet<String> = HashSet::new();
    // Element `id` set per XHTML document, for fragment (RSC-012) resolution.
    // Built across all docs first: a link may target a document not yet visited.
    let mut doc_ids: HashMap<String, HashSet<String>> = HashMap::new();
    // (source_path, raw_href) for every reference carrying a `#fragment`.
    let mut fragment_refs: Vec<(String, String)> = Vec::new();

    for item in &pkg.manifest {
        if !is_xhtml(&item.media_type) {
            continue;
        }
        let path = join_opf(opf_dir, &item.href);
        let Ok(text) = read_text(zip, &path) else {
            continue;
        };
        doc_ids.insert(path.clone(), collect_element_ids(&text));
        for (kind, href) in collect_references(&text) {
            if href.contains('#') {
                fragment_refs.push((path.clone(), href.clone()));
                // RSC-013: a stylesheet reference must not carry a fragment.
                if let Some((file, frag)) = href.split_once('#')
                    && !frag.is_empty()
                    && file
                        .rsplit_once('.')
                        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("css"))
                {
                    report.push(Violation::new(
                        Rule::StylesheetFragment,
                        path.clone(),
                        format!("stylesheet reference {href:?} must not include a fragment"),
                    ));
                }
            }
            // RSC-030: file: URLs are never allowed. `get(..5)` is char-boundary
            // safe (returns None rather than slicing a multi-byte char).
            if href
                .get(..5)
                .is_some_and(|p| p.eq_ignore_ascii_case("file:"))
            {
                report.push(Violation::new(
                    Rule::FileUrlNotAllowed,
                    path.clone(),
                    format!("reference {href:?} uses a file: URL, not allowed in EPUB"),
                ));
            }
            if let Some(resolved) = resolve_href(&path, &href) {
                // RSC-033: a relative URL must not carry a query component. The
                // '?' would otherwise be swallowed into the resolved path and
                // misfire as a broken href, so handle it first and skip the rest.
                if href.split('#').next().unwrap_or(&href).contains('?') {
                    report.push(Violation::new(
                        Rule::RelativeUrlWithQuery,
                        path.clone(),
                        format!("relative reference {href:?} must not have a query component"),
                    ));
                    continue;
                }
                if kind == RefKind::Hyperlink {
                    hyperlink_targets.insert(resolved.clone());
                }
                if !zip_paths.contains(&resolved) {
                    report.push(Violation::new(
                        Rule::BrokenHref,
                        path.clone(),
                        format!("reference {href:?} -> {resolved:?} not present in the zip"),
                    ));
                }
                if escapes_opf_root(opf_dir, &resolved) {
                    report.push(Violation::new(
                        Rule::HrefEscapesOpfRoot,
                        path.clone(),
                        format!(
                            "reference {href:?} resolves to {resolved:?}, outside the OPF root {opf_dir:?}"
                        ),
                    ));
                }
            }
        }
    }

    check_fragments(&doc_ids, &fragment_refs, report);

    // Reachability: every spine item with `linear="no"` must be the target of
    // some hyperlink elsewhere in the publication.
    for s in &pkg.spine {
        if s.linear != Some(false) {
            continue;
        }
        let Some(item) = pkg.manifest_by_id(&s.idref) else {
            continue; // already reported by check_spine_idrefs
        };
        let resolved = join_opf(opf_dir, &item.href);
        if !hyperlink_targets.contains(&resolved) {
            report.push(Violation::new(
                Rule::NonLinearUnreachable,
                opf_path,
                format!(
                    "spine item idref={:?} (href={:?}) has linear=\"no\" but no <a href> in the publication points to it (EPUB 3.3 §5.8.2)",
                    s.idref, resolved
                ),
            ));
        }
    }
}

fn check_parent_paths_in_opf(
    pkg: &opf::Package,
    opf_dir: &str,
    opf_path: &str,
    report: &mut Report,
) {
    for item in &pkg.manifest {
        let resolved = join_opf(opf_dir, &item.href);
        if escapes_opf_root(opf_dir, &resolved) {
            report.push(Violation::new(
                Rule::HrefEscapesOpfRoot,
                opf_path,
                format!(
                    "manifest item href={:?} resolves to {:?}, outside the OPF root {:?}",
                    item.href, resolved, opf_dir
                ),
            ));
        }
    }
}

/// A reference is a hyperlink (feeds reachability) or a resource load. The
/// distinction matters only for reachability (EPUB 3.3 §5.8.2, which is about
/// *hyperlinks*); RSC-007 resolution and the escape check treat both alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefKind {
    Hyperlink,
    Resource,
}

/// Every internal reference in the document, paired with its kind. Covers the
/// reference-bearing element/attribute pairs epubcheck resolves (RSC-007):
/// hyperlinks `<a|area href>`; resources `<link href>`, `<img src>`, SVG
/// `<image|use href>` (incl. `xlink:href`, via attribute local-name),
/// `<object data>`, `<embed|iframe|source|audio|video|track|script src>`, and
/// `<video poster>`. External URLs and data: URIs are filtered later by
/// [`resolve_href`]. `srcset` (multi-URL) and CSS `url()` are out of scope here.
fn collect_references(content: &str) -> Vec<(RefKind, String)> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(false);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name();
                let local = local_name(name.as_ref());
                let kind = match local {
                    b"a" | b"area" => RefKind::Hyperlink,
                    b"link" | b"img" | b"image" | b"use" | b"object" | b"embed" | b"iframe"
                    | b"source" | b"audio" | b"video" | b"track" | b"script" => RefKind::Resource,
                    _ => continue,
                };
                for attr in e.attributes().flatten() {
                    let akey = local_name(attr.key.as_ref());
                    let is_ref = match local {
                        b"a" | b"area" | b"link" | b"image" | b"use" => akey == b"href",
                        b"object" => akey == b"data",
                        b"video" => akey == b"src" || akey == b"poster",
                        _ => akey == b"src",
                    };
                    if is_ref {
                        let val = String::from_utf8_lossy(&attr.value).to_string();
                        if !val.is_empty() {
                            out.push((kind, val));
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break, // XHTML well-formedness is out of scope here.
            _ => {}
        }
    }
    out
}

/// Every `id` attribute value in the document — the fragment-target namespace.
/// Per HTML5 (and epubcheck's ID registry, which reads `getAttribute("id")`),
/// legacy `name=` anchors are *not* counted.
fn collect_element_ids(content: &str) -> HashSet<String> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(false);
    let mut out = HashSet::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() == b"id" {
                        out.insert(String::from_utf8_lossy(&attr.value).to_string());
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break, // well-formedness is out of scope here
            _ => {}
        }
    }
    out
}

/// RSC-012: every `#fragment` must name an element `id` in its target
/// document. Scoped to the local XHTML documents we index; a fragment into an
/// image, stylesheet, SVG, or an entirely missing file (already reported as a
/// broken href / RSC-007) is left alone. A same-document `#frag` resolves
/// against the source document itself.
fn check_fragments(
    doc_ids: &HashMap<String, HashSet<String>>,
    fragment_refs: &[(String, String)],
    report: &mut Report,
) {
    for (source_path, href) in fragment_refs {
        let Some((file, frag)) = href.split_once('#') else {
            continue;
        };
        // An empty fragment (`foo.xhtml#`) is not an id reference; `epubcfi(…)`
        // is a different scheme epubcheck doesn't resolve by id; a
        // percent-encoded fragment we can't compare byte-for-byte without
        // decoding, so we leave it rather than risk a false positive.
        if frag.is_empty() || frag.starts_with("epubcfi(") || frag.contains('%') {
            continue;
        }
        let target = if file.is_empty() {
            source_path.clone() // same-document reference
        } else {
            match resolve_href(source_path, href) {
                Some(t) => t,
                None => continue, // external URL — not ours to resolve
            }
        };
        // Only a document we actually indexed (a local XHTML) can be judged.
        let Some(ids) = doc_ids.get(&target) else {
            continue;
        };
        if !ids.contains(frag) {
            report.push(Violation::new(
                Rule::FragmentNotDefined,
                source_path.clone(),
                format!("href={href:?} points at fragment #{frag}, not defined in {target:?}"),
            ));
        }
    }
}

// =========================================================================
// Path helpers
// =========================================================================

fn join_opf(opf_dir: &str, href: &str) -> String {
    if opf_dir.is_empty() {
        href.to_string()
    } else {
        format!("{}/{}", opf_dir, href)
    }
}

/// Resolve `href` against the directory of `source_path`. Returns `None`
/// for external URLs (`http:`, `mailto:`, …), pure fragments, and empty
/// hrefs. The result is the zip-relative path of the link target with the
/// fragment stripped.
fn resolve_href(source_path: &str, href: &str) -> Option<String> {
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    if href.contains("://")
        || href.starts_with("mailto:")
        || href.starts_with("tel:")
        || href.starts_with("data:")
    {
        return None;
    }
    let no_frag = href.split('#').next().unwrap_or(href);
    if no_frag.is_empty() {
        return None;
    }
    let source_dir = source_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let mut parts: Vec<&str> = if source_dir.is_empty() {
        Vec::new()
    } else {
        source_dir.split('/').collect()
    };
    for seg in no_frag.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// True when `resolved` (a zip-relative path produced by [`resolve_href`] or
/// [`join_opf`]) is outside the OPF root directory. `OEBPS/text/foo.xhtml`
/// with `<a href="../style.css">` resolves to `OEBPS/style.css` (inside
/// `OEBPS/` — fine); the same chapter with `<a href="../../escape.xhtml">`
/// resolves to `escape.xhtml` at zip root (outside `OEBPS/` — flagged).
fn escapes_opf_root(opf_dir: &str, resolved: &str) -> bool {
    if opf_dir.is_empty() {
        return false;
    }
    let prefix = format!("{}/", opf_dir);
    !resolved.starts_with(&prefix) && resolved != opf_dir
}

fn is_xhtml(media_type: &str) -> bool {
    media_type.eq_ignore_ascii_case("application/xhtml+xml")
}

fn read_text(zip: &mut ZipArchive<Cursor<&[u8]>>, name: &str) -> io::Result<String> {
    let mut entry = zip.by_name(name)?;
    let mut s = String::new();
    entry.read_to_string(&mut s)?;
    Ok(s)
}

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|b| *b == b':').next().unwrap_or(name)
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn severity_rank(s: crate::validate::Severity) -> u8 {
        use crate::validate::Severity::*;
        match s {
            Error => 2,
            Warning => 1,
            Info => 0,
        }
    }

    #[test]
    fn all_rules_list_is_complete() {
        assert_eq!(Rule::ALL.len(), 39, "update Rule::ALL when adding a Rule");
    }

    #[test]
    fn every_epubcheck_message_id_exists_in_the_catalog() {
        // An epubcheck id starts uppercase (`RSC-007`); bokai-native ids are
        // lowercase kebab (`nav-missing`). Only the former must be catalogued.
        for &rule in Rule::ALL {
            let id = rule.message_id();
            if id.starts_with(|c: char| c.is_ascii_uppercase()) {
                assert!(
                    messages::lookup(id).is_some(),
                    "rule {rule:?} maps to {id:?}, absent from the epubcheck catalog"
                );
            }
        }
    }

    #[test]
    fn epubcheck_error_coverage_is_tracked() {
        let (covered, total) = epubcheck_error_coverage();
        assert_eq!(total, 138, "error-level catalog total drifted");
        // Distinct epubcheck error-level ids implemented so far; the port's
        // progress ratchet. OPF batch added OPF-030/031/033/034/040/048/050/060/
        // 074/091/096/099; OCF batch PKG-005/009/011/025 + RSC-030; XML batch
        // HTM-001/003 + RSC-013; container/URL batch RSC-003 + RSC-033.
        assert_eq!(
            covered, 34,
            "epubcheck error coverage changed: {covered}/{total}"
        );
    }

    #[test]
    fn bokai_never_rates_a_rule_below_epubcheck() {
        // The parity invariant: for a rule keyed to an epubcheck id, bokai's
        // severity is at least epubcheck's — so an epubcheck ERROR is never
        // downgraded (bokai may be stricter, e.g. OPF-003 USAGE surfaced as
        // Warning).
        for &rule in Rule::ALL {
            let Some(known) = messages::lookup(rule.message_id()) else {
                continue; // bokai-native rule
            };
            if let Some(epub_sev) = known.severity {
                assert!(
                    severity_rank(rule.severity()) >= severity_rank(epub_sev),
                    "{}: bokai {:?} < epubcheck {:?}",
                    rule.message_id(),
                    rule.severity(),
                    epub_sev,
                );
            }
        }
    }

    #[test]
    fn into_findings_maps_severity_check_and_fix() {
        use crate::validate::Severity;
        // Hand-built violations exercise the Rule -> Finding lowering directly.
        let report = Report {
            violations: vec![
                Violation::new(Rule::FileNotInManifest, "OEBPS/content.opf", "extra file"),
                Violation::new(Rule::NavMissing, "OEBPS/content.opf", "no nav"),
                Violation::new(Rule::BrokenHref, "OEBPS/text/ch1.xhtml", "dangling"),
            ],
        };
        let findings = report.into_findings();
        assert_eq!(findings.len(), 3);
        assert!(findings.iter().all(|f| f.check == "epub"));

        assert_eq!(findings[0].rule, "OPF-003"); // FileNotInManifest
        assert_eq!(findings[0].severity, Severity::Warning);
        assert!(findings[0].fix.is_none());

        assert_eq!(findings[1].rule, "nav-missing"); // bokai-native
        assert_eq!(findings[1].severity, Severity::Error);
        assert_eq!(findings[1].fix.as_ref().unwrap().action, "add-nav-doc");

        assert_eq!(findings[2].rule, "RSC-007"); // BrokenHref
        assert_eq!(findings[2].severity, Severity::Error);
        assert!(findings[2].fix.is_none());
    }

    #[test]
    fn content_conformance_rules_lower_to_findings() {
        let report = Report {
            violations: vec![
                Violation::new(Rule::IrregularDoctype, "OEBPS/c1.xhtml", "xhtml 1.1"),
                Violation::new(Rule::EmptyTitle, "OEBPS/c1.xhtml", "empty"),
            ],
        };
        let f = report.into_findings();
        assert_eq!(f[0].rule, "HTM-004"); // IrregularDoctype
        assert_eq!(f[0].fix.as_ref().unwrap().action, "use-html5-doctype");
        assert_eq!(f[1].rule, "RSC-005"); // EmptyTitle (schema-error channel)
        assert_eq!(f[1].fix.as_ref().unwrap().action, "fill-title");
    }

    #[test]
    fn find_doctype_matches_casings_and_is_utf8_safe() {
        assert_eq!(
            find_doctype("<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.1//EN\" \"x\">\n<html>"),
            Some("<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.1//EN\" \"x\">")
        );
        assert_eq!(
            find_doctype("<!doctype html>\n<html>"),
            Some("<!doctype html>")
        );
        assert_eq!(find_doctype("<html>字字字</html>"), None);
        // A DOCTYPE past a run of multi-byte chars must be found, not panic.
        let s = format!("{}\n<!DOCTYPE html>", "字".repeat(1000));
        assert_eq!(find_doctype(&s), Some("<!DOCTYPE html>"));
    }

    #[test]
    fn has_empty_title_detects_empty_and_self_closing() {
        assert!(has_empty_title("<head><title></title></head>"));
        assert!(has_empty_title("<head><title/></head>"));
        assert!(has_empty_title("<head><title>   </title></head>"));
        assert!(!has_empty_title("<head><title>第一章</title></head>"));
        assert!(!has_empty_title("<head></head>")); // absent title is a different rule
    }

    #[test]
    fn collect_element_ids_reads_id_not_name() {
        let ids = collect_element_ids(
            r#"<html><body><h1 id="top">t</h1><a name="legacy">x</a><p id="第一"/></body></html>"#,
        );
        assert!(ids.contains("top"));
        assert!(ids.contains("第一")); // multi-byte id round-trips
        assert!(!ids.contains("legacy")); // name= is not a fragment target (HTML5)
    }

    #[test]
    fn check_fragments_flags_dangling_only() {
        let mut doc_ids: HashMap<String, HashSet<String>> = HashMap::new();
        doc_ids.insert("OEBPS/a.xhtml".into(), HashSet::from(["sec1".to_string()]));
        doc_ids.insert("OEBPS/b.xhtml".into(), HashSet::from(["real".to_string()]));
        let refs = vec![
            ("OEBPS/a.xhtml".to_string(), "b.xhtml#real".to_string()), // ok — cross-doc
            ("OEBPS/a.xhtml".to_string(), "#sec1".to_string()),        // ok — same-doc
            ("OEBPS/a.xhtml".to_string(), "b.xhtml#ghost".to_string()), // RSC-012
            ("OEBPS/a.xhtml".to_string(), "#missing".to_string()),     // RSC-012 (same-doc)
            ("OEBPS/a.xhtml".to_string(), "img.svg#x".to_string()),    // target unindexed → skip
            ("OEBPS/a.xhtml".to_string(), "b.xhtml#".to_string()),     // empty fragment → skip
            ("OEBPS/a.xhtml".to_string(), "http://x/y#z".to_string()), // external → skip
            ("OEBPS/a.xhtml".to_string(), "b.xhtml#foo%20bar".to_string()), // percent → skip
            (
                "OEBPS/a.xhtml".to_string(),
                "b.xhtml#epubcfi(/6)".to_string(),
            ), // cfi → skip
        ];
        let mut report = Report::default();
        check_fragments(&doc_ids, &refs, &mut report);
        let n = report
            .violations
            .iter()
            .filter(|v| v.rule == Rule::FragmentNotDefined)
            .count();
        assert_eq!(n, 2, "expected 2 dangling fragments, got:\n{report}");
    }

    #[test]
    fn collect_references_marks_hyperlinks_and_covers_resources() {
        let refs = collect_references(
            r##"<html><body>
                <a href="ch2.xhtml">next</a>
                <area href="map.xhtml"/>
                <img src="pic.png"/>
                <link href="style.css"/>
                <object data="widget.xml"></object>
                <video src="v.mp4" poster="p.png"></video>
                <svg><image xlink:href="s.png"/><use href="#g"/></svg>
                <p>plain</p>
            </body></html>"##,
        );
        // Only <a>/<area> are hyperlinks (the reachability signal).
        let hyper: Vec<&str> = refs
            .iter()
            .filter(|(k, _)| *k == RefKind::Hyperlink)
            .map(|(_, h)| h.as_str())
            .collect();
        assert_eq!(hyper, vec!["ch2.xhtml", "map.xhtml"]);
        // Resource references from every covered element/attribute are present,
        // including SVG `xlink:href` (matched by attribute local-name).
        let all: Vec<&str> = refs.iter().map(|(_, h)| h.as_str()).collect();
        for expect in [
            "pic.png",
            "style.css",
            "widget.xml",
            "v.mp4",
            "p.png",
            "s.png",
            "#g",
        ] {
            assert!(all.contains(&expect), "missing {expect:?} in {all:?}");
        }
    }

    #[test]
    fn opf_structure_flags_the_batch() {
        // One OPF that trips nine structural rules at once.
        let opf = r##"<package unique-identifier="bad-ref">
          <metadata><dc:identifier id="pub-id">x</dc:identifier></metadata>
          <manifest>
            <item id="c" href="c.xhtml" media-type="application/xhtml+xml"/>
            <item id="c2" href="c.xhtml" media-type="application/xhtml+xml"/>
            <item id="frag" href="d.xhtml#x" media-type="application/xhtml+xml"/>
            <item id="self" href="content.opf" media-type="application/oebps-package+xml"/>
            <item id="img" href="i.svg" media-type="image/svg+xml" fallback="nope"/>
            <item id="ncx" href="t.ncx" media-type="application/xhtml+xml"/>
          </manifest>
          <spine toc="ncx">
            <itemref idref="c" linear="no"/>
            <itemref idref="c" linear="no"/>
          </spine>
          <guide><reference type="x" href="missing.xhtml"/></guide>
        </package>"##;
        let pkg = opf::parse(opf).unwrap();
        let mut report = Report::default();
        check_opf_structure(&pkg, "", "content.opf", &mut report);
        for rule in [
            Rule::UniqueIdentifierNotFound,    // OPF-030
            Rule::SpineNoLinear,               // OPF-033
            Rule::SpineDuplicateIdref,         // OPF-034
            Rule::DuplicateManifestResource,   // OPF-074
            Rule::ManifestListsPackageDoc,     // OPF-099
            Rule::ItemHrefHasFragment,         // OPF-091
            Rule::FallbackNotFound,            // OPF-040
            Rule::SpineTocNotNcx,              // OPF-050
            Rule::GuideReferenceNotInManifest, // OPF-031
        ] {
            assert!(report.has_rule(rule), "expected {rule:?}, got:\n{report}");
        }
    }

    #[test]
    fn opf_structure_flags_missing_unique_identifier() {
        let pkg = opf::parse(
            r#"<package><manifest>
                 <item id="a" href="a.xhtml" media-type="application/xhtml+xml"/>
               </manifest><spine><itemref idref="a"/></spine></package>"#,
        )
        .unwrap();
        let mut report = Report::default();
        check_opf_structure(&pkg, "", "content.opf", &mut report);
        assert!(report.has_rule(Rule::UniqueIdentifierMissing));
        // A well-formed spine (default-linear) must NOT trip OPF-033.
        assert!(!report.has_rule(Rule::SpineNoLinear));
    }

    #[test]
    fn duplicate_zip_entries_are_flagged_once_each() {
        let mut report = Report::default();
        check_duplicate_zip_entries(
            &[
                "a.xhtml".into(),
                "b.xhtml".into(),
                "a.xhtml".into(),
                "a.xhtml".into(),
            ],
            &mut report,
        );
        assert_eq!(
            report
                .violations
                .iter()
                .filter(|v| v.rule == Rule::DuplicateZipEntry)
                .count(),
            1,
            "one finding per duplicated name"
        );
    }

    #[test]
    fn ocf_filename_rules_flag_bad_names_only() {
        let mut report = Report::default();
        check_ocf_filenames(
            &[
                "OEBPS/text/ch1.xhtml".into(), // fine
                "OEBPS/图片/封面.jpg".into(),  // CJK is fine (not control/restricted)
                "OEBPS/a<b>.xhtml".into(),     // PKG-009: '<' and '>'
                "OEBPS/dir./x.xhtml".into(),   // PKG-011: segment "dir." ends with '.'
            ],
            &mut report,
        );
        assert!(report.has_rule(Rule::FilenameForbiddenChar));
        assert!(report.has_rule(Rule::FilenameTrailingDot));
        // The two clean names produced no findings.
        assert_eq!(report.violations.len(), 2, "unexpected extras:\n{report}");
    }

    #[test]
    fn xml_declaration_version_parsing() {
        assert_eq!(
            xml_declaration_version(r#"<?xml version="1.0" encoding="utf-8"?><html/>"#),
            Some("1.0")
        );
        assert_eq!(
            xml_declaration_version("<?xml version='1.1'?>\n<html/>"),
            Some("1.1")
        );
        assert_eq!(xml_declaration_version("<html/>"), None);
        // Must not slice a multi-byte char that precedes a later `<?xml`.
        assert_eq!(xml_declaration_version("字字字<html/>"), None);
    }

    #[test]
    fn xml_conformance_flags_bad_version_and_external_entity() {
        let mut report = Report::default();
        check_xml_conformance(
            r#"<?xml version="1.1"?><!DOCTYPE x [ <!ENTITY ext SYSTEM "http://evil/x"> ]><x/>"#,
            "OEBPS/x.xhtml",
            &mut report,
        );
        assert!(report.has_rule(Rule::XmlVersionNot10)); // HTM-001
        assert!(report.has_rule(Rule::ExternalEntity)); // HTM-003
    }

    #[test]
    fn external_entity_detection_is_scoped() {
        assert!(has_external_entity(r#"<!ENTITY x SYSTEM "a.dtd">"#));
        assert!(has_external_entity(r#"<!ENTITY x PUBLIC "id" "a.dtd">"#));
        // Internal (value) entity is fine; the word SYSTEM elsewhere is fine.
        assert!(!has_external_entity(r#"<!ENTITY copy "&#169;">"#));
        assert!(!has_external_entity("<p>SYSTEM shutdown</p>"));
    }

    #[test]
    fn is_xml_based_matches_plus_xml_family() {
        assert!(is_xml_based("application/xhtml+xml"));
        assert!(is_xml_based("image/svg+xml"));
        assert!(is_xml_based("application/x-dtbncx+xml"));
        assert!(is_xml_based("APPLICATION/XML"));
        assert!(!is_xml_based("text/css"));
        assert!(!is_xml_based("image/jpeg"));
    }

    /// Everything below validates a real EPUB, and the only EPUB *writer* in
    /// the crate is the Aozora builder — so these tests exist only in a build
    /// that has it.
    #[cfg(feature = "aozora")]
    mod on_a_generated_epub {
        use super::super::*;
        use crate::formats::aozora::{Document, EpubInput, TocEntry, build_epub};

        fn tiny_jpeg() -> Vec<u8> {
            // Same 1x1 JPEG used by the epub_builder tests — small, valid header.
            vec![
                0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00,
                0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
            ]
        }

        fn sample_aozora_epub() -> Vec<u8> {
            let doc = Document {
                title: "テスト".to_string(),
                author: "著者".to_string(),
                body_xhtml: r#"<p>序文</p><h2 id="h1">第一章</h2><p>本文</p>"#.to_string(),
                toc: vec![TocEntry {
                    id: "h1".to_string(),
                    level: 2,
                    text: "第一章".to_string(),
                }],
                colophon: String::new(),
                referenced_images: vec![],
            };
            build_epub(EpubInput {
                document: &doc,
                images: &[],
                cover_jpeg: &tiny_jpeg(),
            })
            .unwrap()
        }

        #[test]
        fn aozora_output_passes_after_fix() {
            let bytes = sample_aozora_epub();
            let report = validate(&bytes);
            assert!(
                report.is_clean(),
                "aozora epub should validate clean; got:\n{}",
                report
            );
        }

        #[test]
        fn detects_non_linear_cover_without_hyperlink() {
            // Re-introduce the bug locally: take a valid aozora epub and rewrite
            // the OPF to set `linear="no"` on the cover. Validator must catch it.
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "OEBPS/content.opf", |opf| {
                opf.replace(
                    r#"<itemref idref="cover"/>"#,
                    r#"<itemref idref="cover" linear="no"/>"#,
                )
            });
            let report = validate(&mutated);
            assert!(
                report.has_rule(Rule::NonLinearUnreachable),
                "expected NonLinearUnreachable, got:\n{}",
                report
            );
        }

        #[test]
        fn detects_missing_manifest_file() {
            // Rewrite OPF to reference a nonexistent file.
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "OEBPS/content.opf", |opf| {
                opf.replace(
                    r#"<item id="style" href="style.css" media-type="text/css"/>"#,
                    r#"<item id="style" href="nope.css" media-type="text/css"/>"#,
                )
            });
            let report = validate(&mutated);
            assert!(
                report.has_rule(Rule::ManifestFileMissing),
                "expected ManifestFileMissing, got:\n{}",
                report
            );
        }

        #[test]
        fn detects_broken_image_reference() {
            // Broadened RSC-007: a missing `<img src>` (not just `<a href>`)
            // must be caught. The clean epub validating (aozora_output_passes…)
            // is the paired proof the cover image / stylesheet don't false-fire.
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "OEBPS/text/title.xhtml", |xhtml| {
                xhtml.replace("</body>", r#"<img src="missing.png"/></body>"#)
            });
            let report = validate(&mutated);
            assert!(
                report.has_rule(Rule::BrokenHref),
                "expected BrokenHref for missing image, got:\n{}",
                report
            );
        }

        #[test]
        fn detects_relative_url_with_query() {
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "OEBPS/text/title.xhtml", |x| {
                x.replace("</body>", r##"<a href="body.xhtml?x=1">q</a></body>"##)
            });
            let report = validate(&mutated);
            assert!(
                report.has_rule(Rule::RelativeUrlWithQuery),
                "expected RSC-033, got:\n{report}"
            );
        }

        #[test]
        fn detects_rootfile_without_ops_media_type() {
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "META-INF/container.xml", |c| {
                c.replace("application/oebps-package+xml", "application/xml")
            });
            let report = validate(&mutated);
            assert!(
                report.has_rule(Rule::NoOpsRootfile),
                "expected RSC-003, got:\n{report}"
            );
        }

        #[test]
        fn detects_dangling_fragment() {
            // A same-document `#frag` naming no element id must trip RSC-012.
            // (The untouched epub validating clean — `aozora_output_passes_after_fix`
            // — is the paired proof that resolved fragments do *not* fire.)
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "OEBPS/text/title.xhtml", |xhtml| {
                xhtml.replace("</body>", r##"<a href="#does-not-exist">x</a></body>"##)
            });
            let report = validate(&mutated);
            assert!(
                report.has_rule(Rule::FragmentNotDefined),
                "expected FragmentNotDefined, got:\n{}",
                report
            );
        }

        #[test]
        fn detects_href_escaping_opf_root() {
            // OPF root is `OEBPS/`. Insert a `../../escape.xhtml` from a chapter:
            // resolves to `escape.xhtml` (zip root, outside OEBPS) — must flag.
            // (A plain `../style.css` would resolve to `OEBPS/style.css` and is
            // therefore fine — verified by `aozora_output_passes_after_fix`.)
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "OEBPS/text/title.xhtml", |xhtml| {
                xhtml.replace("</body>", r#"<a href="../../escape.xhtml">link</a></body>"#)
            });
            let report = validate(&mutated);
            assert!(
                report.has_rule(Rule::HrefEscapesOpfRoot),
                "expected HrefEscapesOpfRoot, got:\n{}",
                report
            );
        }

        #[test]
        fn source_validate_is_clean_for_aozora_epub() {
            let bytes = sample_aozora_epub();
            let report = crate::validate::source::validate(&bytes);
            assert!(
                report.is_clean(),
                "aozora epub should be clean through source::validate; got:\n{report}"
            );
        }

        #[test]
        fn source_validate_surfaces_epub_defect() {
            // A manifest-file-missing epub must surface as an `epub` Finding via the
            // source aggregator — proving it wires the epub check's lowering.
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "OEBPS/content.opf", |opf| {
                opf.replace(
                    r#"<item id="style" href="style.css" media-type="text/css"/>"#,
                    r#"<item id="style" href="nope.css" media-type="text/css"/>"#,
                )
            });
            let report = crate::validate::source::validate(&mutated);
            assert!(!report.is_clean());
            assert!(
                report.findings.iter().any(|f| f.check == "epub"
                    && f.rule == "RSC-001"
                    && f.severity == crate::validate::Severity::Error),
                "expected an epub/RSC-001 (manifest file missing) error, got:\n{report}"
            );
        }

        /// Rebuild `epub_bytes` with `entry`'s content rewritten by `f`. Uses the
        /// `zip` crate to iterate entries and emit a new archive with the same
        /// per-entry compression methods. Preserves mimetype-first ordering.
        fn rewrite_zip_entry(
            epub_bytes: &[u8],
            target: &str,
            f: impl Fn(String) -> String,
        ) -> Vec<u8> {
            use zip::write::{SimpleFileOptions, ZipWriter};
            let cursor = Cursor::new(epub_bytes);
            let mut zin = ZipArchive::new(cursor).unwrap();
            let mut out = Vec::with_capacity(epub_bytes.len());
            let mut zout = ZipWriter::new(Cursor::new(&mut out));
            for i in 0..zin.len() {
                let mut e = zin.by_index(i).unwrap();
                let name = e.name().to_string();
                let comp = e.compression();
                let opts: SimpleFileOptions = SimpleFileOptions::default().compression_method(comp);
                let mut buf = Vec::new();
                e.read_to_end(&mut buf).unwrap();
                zout.start_file(&name, opts).unwrap();
                if name == target {
                    let s = String::from_utf8(buf).unwrap();
                    let rewritten = f(s);
                    std::io::Write::write_all(&mut zout, rewritten.as_bytes()).unwrap();
                } else {
                    std::io::Write::write_all(&mut zout, &buf).unwrap();
                }
            }
            zout.finish().unwrap();
            out
        }
    }
}
