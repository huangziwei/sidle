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
//!   included (epubcheck RSC-012). Fragments into SVG/other targets and `srcset`
//!   are not yet indexed.
//! - Every `url()`/`@import` reference in a CSS resource resolves to a file in
//!   the zip (RSC-007) and stays inside the container (RSC-026), resolved
//!   relative to the stylesheet. Inline `<style>`/`style=` CSS is not yet scanned.
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

    /// Any error-level violation. This — not [`is_clean`](Self::is_clean), which
    /// is true only with zero violations of *any* severity — is the predicate
    /// that stands in for "would epubcheck reject this?": epubcheck exits 0 on
    /// warnings, so an EPUB with only warning violations is still
    /// epubcheck-valid. The conversion/repair flags key on this; warnings are
    /// surfaced but never treated as invalidity.
    pub fn has_errors(&self) -> bool {
        self.violations
            .iter()
            .any(|v| v.rule.severity() == crate::validate::Severity::Error)
    }

    /// How many violations sit at the given unified severity.
    pub fn count(&self, severity: crate::validate::Severity) -> usize {
        self.violations
            .iter()
            .filter(|v| v.rule.severity() == severity)
            .count()
    }

    /// A [`Display`] view of only the error-level violations, for the
    /// conversion/repair flags (which key on errors, not warnings — see
    /// [`has_errors`](Self::has_errors)). Renders the same per-violation lines
    /// as the full report, so a diagnostic shows exactly what makes the EPUB
    /// invalid without the surrounding warning noise.
    pub fn errors_display(&self) -> ErrorsDisplay<'_> {
        ErrorsDisplay(self)
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

/// A [`Display`] wrapper over a [`Report`] that prints only its error-level
/// violations. Built by [`Report::errors_display`].
pub struct ErrorsDisplay<'a>(&'a Report);

impl fmt::Display for ErrorsDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for v in self
            .0
            .violations
            .iter()
            .filter(|v| v.rule.severity() == crate::validate::Severity::Error)
        {
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
    MimetypeBadContent,
    MissingContainerXml,
    OpfMissing,
    OpfParseError,
    PackageVersionMissing,
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
    // Cross-document identity & reference integrity.
    NcxUidMismatch,
    ReferenceNotInManifest,
    DoctypeExternalIdentifier,
    GuideReferenceNotContentDoc,
    // Character encoding (epubcheck RSC-028 / HTM-058 / RSC-027).
    XmlEncodingNotUtf8,
    XhtmlEncodingUtf16,
    XmlEncodingUtf16,
    // Container rootfile & manifest fallback integrity (epubcheck OPF-*).
    RootfileMissingFullPath,
    RootfileEmptyFullPath,
    SpineItemNoFallback,
    SpineItemFallbackNotContentDoc,
    FallbackChainCircular,
    // Dublin Core metadata values (epubcheck OPF-* / RSC-005 schema channel).
    MissingTitle,
    MissingLanguage,
    IdentifierInvalidUuid,
    DateSyntaxNotRecommended,
    DateNotValid,
    LanguageTagNotWellFormed,
    // Remote / data URLs in the manifest (epubcheck RSC-006 / RSC-029).
    RemoteResourceInSpine,
    RemoteResourceNotAllowed,
    DataUrlNotAllowed,
    // Foreign-resource fallback rules (epubcheck RSC-032 / MED-003).
    ForeignResourceNoFallback,
    PictureImgNotCoreType,
    // Package `<link>` element rules (epubcheck OPF-089/095/098/067).
    LinkAlternatePaired,
    VoicingLinkNotAudio,
    LinkIntoPackageDocument,
    LinkToNonSpineManifestItem,
    // Hyperlink target integrity (epubcheck RSC-010 / RSC-011).
    HyperlinkToNonContentDocument,
    HyperlinkToNonSpineItem,
    // Navigation document (epubcheck NAV-010).
    NavRemoteLink,
    // OCF font obfuscation (epubcheck PKG-026).
    ObfuscatedResourceNotFont,
    // Bitmap header vs declared media-type (epubcheck OPF-029).
    ResourceMediaTypeMismatch,
    // Manifest property declaration vs content (epubcheck OPF-014).
    PropertyNotDeclared,
    // EPUB 3 vocabulary prefix declaration (epubcheck OPF-028).
    UndeclaredPrefix,
    // Fixed-layout content document viewport (epubcheck HTM-046).
    FixedLayoutNoViewport,
    // Undecodable raster image (epubcheck PKG-021).
    CorruptImage,
    // Reference URL not valid per the WHATWG URL standard (epubcheck RSC-020).
    InvalidUrl,
    // XML document is not well-formed / namespace-well-formed (epubcheck RSC-016).
    MalformedXml,
    // An attribute the package RelaxNG grammar forbids (epubcheck RSC-005 schema
    // channel): an OPF-namespaced attribute in EPUB 3, or a non-`id`/`toc`
    // attribute on `<spine>` in EPUB 2.
    DisallowedOpfAttribute,
    // The rest of the RSC-005 schema channel that needs no grammar engine:
    // `id` lexical form and uniqueness, the EPUB 2 content-document grammar,
    // the NCX schematron, the nav schematron, and package metadata content.
    /// Fixed-layout viewport metadata (`HTM-047/056/057/059`) and the SVG
    /// fixed-layout `viewBox` (`HTM-048`).
    ViewportSyntax,
    ViewportDimensionMissing,
    ViewportValueInvalid,
    ViewportPropertyRepeated,
    SvgFixedLayoutNoViewBox,
    /// Stylesheet-facing content rules: an untitled alternate stylesheet
    /// (`CSS-015`) and an empty CSS `url()` (`CSS-002`).
    AlternateStylesheetNoTitle,
    EmptyCssReference,
    /// An SVG `<use>`/paint/clip-path reference without the fragment it needs
    /// to name a target (`RSC-015`).
    SvgReferenceNoFragment,
    /// A manifest `fallback-style` that names no declared item (`OPF-041`).
    FallbackStyleNotFound,
    IdNotNcName,
    DuplicateId,
    MissingRequiredAttribute,
    DisallowedContentAttribute,
    UndeclaredContentElement,
    UriAttributeInvalid,
    MetaEncodingDeclaration,
    BodyRoleNotAllowed,
    NestedHyperlink,
    NcxPlayOrder,
    NavTocOccurrence,
    EmptyDcContent,
    MissingIdentifier,
    UnresolvedUniqueIdentifier,
    SpineTocMissing,
    PackageIncomplete,
}

impl Rule {
    /// Every variant, so callers (coverage reporting, the mapping-invariant
    /// tests) can iterate the rule set. `message_id`'s match is
    /// compiler-exhaustive; keep this in sync (a test asserts the length).
    pub const ALL: &'static [Rule] = &[
        Rule::ZipMalformed,
        Rule::MimetypeNotFirst,
        Rule::MimetypeBadContent,
        Rule::MissingContainerXml,
        Rule::OpfMissing,
        Rule::OpfParseError,
        Rule::PackageVersionMissing,
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
        Rule::NcxUidMismatch,
        Rule::ReferenceNotInManifest,
        Rule::DoctypeExternalIdentifier,
        Rule::GuideReferenceNotContentDoc,
        Rule::XmlEncodingNotUtf8,
        Rule::XhtmlEncodingUtf16,
        Rule::XmlEncodingUtf16,
        Rule::RootfileMissingFullPath,
        Rule::RootfileEmptyFullPath,
        Rule::SpineItemNoFallback,
        Rule::SpineItemFallbackNotContentDoc,
        Rule::FallbackChainCircular,
        Rule::MissingTitle,
        Rule::MissingLanguage,
        Rule::IdentifierInvalidUuid,
        Rule::DateSyntaxNotRecommended,
        Rule::DateNotValid,
        Rule::LanguageTagNotWellFormed,
        Rule::RemoteResourceInSpine,
        Rule::RemoteResourceNotAllowed,
        Rule::DataUrlNotAllowed,
        Rule::ForeignResourceNoFallback,
        Rule::PictureImgNotCoreType,
        Rule::LinkAlternatePaired,
        Rule::VoicingLinkNotAudio,
        Rule::LinkIntoPackageDocument,
        Rule::LinkToNonSpineManifestItem,
        Rule::HyperlinkToNonContentDocument,
        Rule::HyperlinkToNonSpineItem,
        Rule::NavRemoteLink,
        Rule::ObfuscatedResourceNotFont,
        Rule::ResourceMediaTypeMismatch,
        Rule::PropertyNotDeclared,
        Rule::UndeclaredPrefix,
        Rule::FixedLayoutNoViewport,
        Rule::CorruptImage,
        Rule::InvalidUrl,
        Rule::MalformedXml,
        Rule::DisallowedOpfAttribute,
        Rule::ViewportSyntax,
        Rule::ViewportDimensionMissing,
        Rule::ViewportValueInvalid,
        Rule::ViewportPropertyRepeated,
        Rule::SvgFixedLayoutNoViewBox,
        Rule::AlternateStylesheetNoTitle,
        Rule::EmptyCssReference,
        Rule::SvgReferenceNoFragment,
        Rule::FallbackStyleNotFound,
        Rule::IdNotNcName,
        Rule::DuplicateId,
        Rule::MissingRequiredAttribute,
        Rule::DisallowedContentAttribute,
        Rule::UndeclaredContentElement,
        Rule::UriAttributeInvalid,
        Rule::MetaEncodingDeclaration,
        Rule::BodyRoleNotAllowed,
        Rule::NestedHyperlink,
        Rule::NcxPlayOrder,
        Rule::NavTocOccurrence,
        Rule::EmptyDcContent,
        Rule::MissingIdentifier,
        Rule::UnresolvedUniqueIdentifier,
        Rule::SpineTocMissing,
        Rule::PackageIncomplete,
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
            Rule::MimetypeBadContent => "PKG-007",
            Rule::MissingContainerXml => "RSC-002",
            Rule::OpfMissing => "OPF-002",
            Rule::OpfParseError => "RSC-005",
            Rule::PackageVersionMissing => "OPF-001",
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
            Rule::NcxUidMismatch => "NCX-001",
            Rule::ReferenceNotInManifest => "RSC-008",
            Rule::DoctypeExternalIdentifier => "OPF-073",
            Rule::GuideReferenceNotContentDoc => "OPF-032",
            Rule::XmlEncodingNotUtf8 => "RSC-028",
            Rule::XhtmlEncodingUtf16 => "HTM-058",
            Rule::XmlEncodingUtf16 => "RSC-027",
            Rule::RootfileMissingFullPath => "OPF-016",
            Rule::RootfileEmptyFullPath => "OPF-017",
            Rule::SpineItemNoFallback => "OPF-043",
            Rule::SpineItemFallbackNotContentDoc => "OPF-044",
            Rule::FallbackChainCircular => "OPF-045",
            // Missing required Dublin Core metadata is enforced by epubcheck's
            // package schema, which reports through the RSC-005 channel.
            Rule::MissingTitle => "RSC-005",
            Rule::MissingLanguage => "RSC-005",
            Rule::IdentifierInvalidUuid => "OPF-085",
            Rule::DateSyntaxNotRecommended => "OPF-053",
            Rule::DateNotValid => "OPF-054",
            Rule::LanguageTagNotWellFormed => "OPF-092",
            Rule::RemoteResourceInSpine => "RSC-006",
            Rule::RemoteResourceNotAllowed => "RSC-006",
            Rule::DataUrlNotAllowed => "RSC-029",
            Rule::ForeignResourceNoFallback => "RSC-032",
            Rule::PictureImgNotCoreType => "MED-003",
            Rule::LinkAlternatePaired => "OPF-089",
            Rule::VoicingLinkNotAudio => "OPF-095",
            Rule::LinkIntoPackageDocument => "OPF-098",
            Rule::LinkToNonSpineManifestItem => "OPF-067",
            Rule::HyperlinkToNonContentDocument => "RSC-010",
            Rule::HyperlinkToNonSpineItem => "RSC-011",
            Rule::NavRemoteLink => "NAV-010",
            Rule::ObfuscatedResourceNotFont => "PKG-026",
            Rule::ResourceMediaTypeMismatch => "OPF-029",
            Rule::PropertyNotDeclared => "OPF-014",
            Rule::UndeclaredPrefix => "OPF-028",
            Rule::FixedLayoutNoViewport => "HTM-046",
            Rule::CorruptImage => "PKG-021",
            Rule::InvalidUrl => "RSC-020",
            Rule::MalformedXml => "RSC-016",
            Rule::DisallowedOpfAttribute => "RSC-005",
            Rule::ViewportSyntax => "HTM-047",
            Rule::ViewportDimensionMissing => "HTM-056",
            Rule::ViewportValueInvalid => "HTM-057",
            Rule::ViewportPropertyRepeated => "HTM-059",
            Rule::SvgFixedLayoutNoViewBox => "HTM-048",
            Rule::AlternateStylesheetNoTitle => "CSS-015",
            Rule::EmptyCssReference => "CSS-002",
            Rule::SvgReferenceNoFragment => "RSC-015",
            Rule::FallbackStyleNotFound => "OPF-041",
            Rule::IdNotNcName => "RSC-005",
            Rule::DuplicateId => "RSC-005",
            Rule::MissingRequiredAttribute => "RSC-005",
            Rule::DisallowedContentAttribute => "RSC-005",
            Rule::UndeclaredContentElement => "RSC-005",
            Rule::UriAttributeInvalid => "RSC-005",
            Rule::MetaEncodingDeclaration => "RSC-005",
            Rule::BodyRoleNotAllowed => "RSC-005",
            Rule::NestedHyperlink => "RSC-005",
            Rule::NcxPlayOrder => "RSC-005",
            Rule::NavTocOccurrence => "RSC-005",
            Rule::EmptyDcContent => "RSC-005",
            Rule::MissingIdentifier => "RSC-005",
            Rule::UnresolvedUniqueIdentifier => "RSC-005",
            Rule::SpineTocMissing => "RSC-005",
            Rule::PackageIncomplete => "RSC-005",
        }
    }

    /// Severity of this rule in the unified [`crate::validate::Severity`] model.
    /// Undeclared extra resources are real but widely tolerated (Warning);
    /// every other rule either violates a MUST or corrupts conversion (Error).
    fn severity(self) -> crate::validate::Severity {
        use crate::validate::Severity;
        match self {
            // The rules epubcheck rates below Error: undeclared extra resources
            // (OPF-003), UTF-16 in a non-XHTML resource (RSC-027), an invalid
            // UUID identifier (OPF-085), and a non-recommended date syntax in
            // EPUB 3 (OPF-053).
            Rule::FileNotInManifest
            | Rule::XmlEncodingUtf16
            | Rule::IdentifierInvalidUuid
            | Rule::DateSyntaxNotRecommended => Severity::Warning,
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

    // OCF entry names are always UTF-8 (EPUB spec); decode the raw name bytes as
    // UTF-8 rather than trusting the zip crate's `name()`, which falls back to
    // CP437 for entries lacking the language-encoding (EFS) flag — a common way
    // real tools store UTF-8 names, which epubcheck reads as UTF-8 regardless.
    let zip_names: Vec<String> = (0..zip.len())
        .filter_map(|i| {
            zip.by_index(i)
                .ok()
                .map(|f| nfc(&String::from_utf8_lossy(f.name_raw())))
        })
        .collect();
    check_duplicate_zip_entries(&zip_names, &mut report);
    check_ocf_filenames(&zip_names, &mut report);
    let zip_paths: HashSet<String> = zip_names.into_iter().collect();
    let opf_dir = opf_path
        .rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default();

    // OPF-001: the package element must declare a version. Without it epubcheck
    // cannot determine EPUB 2 vs 3, so the version-gated checks below stay off.
    if pkg.version.is_none() {
        report.push(Violation::new(
            Rule::PackageVersionMissing,
            opf_path.clone(),
            "package version attribute not found",
        ));
    }

    let epub2 = is_epub2(&pkg);
    let epub3 = is_epub3(&pkg);
    // Resources `META-INF/encryption.xml` encrypts (font obfuscation included):
    // their stored bytes are not the real content, and epubcheck can decrypt
    // none of them, so it skips every content check on them (RSC-004).
    let encrypted = read_text(&mut zip, "META-INF/encryption.xml")
        .map(|text| encrypted_uris(&text))
        .unwrap_or_default();
    check_manifest_files(&pkg, &opf_dir, &zip_paths, &opf_path, &mut report);
    check_files_in_manifest(&pkg, &opf_dir, &zip_paths, &opf_path, &mut report);
    check_spine_idrefs(&pkg, &opf_path, &mut report);
    check_nav_present(&pkg, epub3, &opf_path, &mut report);
    check_parent_paths_in_opf(&pkg, &opf_dir, &opf_path, &mut report);
    check_opf_url_validity(&pkg, &opf_path, &mut report);
    check_opf_structure(&pkg, &opf_dir, &opf_path, epub2, epub3, &mut report);
    check_fallback_chain_and_spine(&pkg, epub2, epub3, &opf_path, &mut report);
    // RSC-005 schema channel over the package metadata and structure (both
    // versions).
    check_dc_content(&pkg, epub2, epub3, &opf_path, &mut report);
    check_package_schema_structure(&pkg, epub2, epub3, &opf_path, &mut report);
    // Dublin Core metadata value rules key on bokai's EPUB 3 understanding of
    // `<dc:*>`; a version-less/legacy package (OEB 1.x) uses a different
    // structure epubcheck rejects with OPF-001, so gate to EPUB 3.
    if epub3 {
        check_metadata(&pkg, epub2, &opf_path, &mut report);
        // OPF-028: EPUB 3 vocabulary-prefix declarations (the prefix mechanism is
        // EPUB 3's; a version-less/legacy package is rejected via OPF-001).
        check_prefix_declarations(&pkg, &opf_path, &mut report);
        // RSC-006/029 and the package <link> rules are EPUB 3 (epubcheck's
        // OPFChecker30 / OPFHandler30).
        check_remote_and_data_urls(&pkg, &opf_path, &mut report);
        check_remote_references(&pkg, &opf_dir, &mut zip, &encrypted, &mut report);
        check_foreign_resources(&pkg, &opf_dir, &mut zip, &encrypted, &mut report);
        check_opf_links(&pkg, &opf_dir, &opf_path, &mut report);
        // RSC-005 schema channel: the EPUB 3 nav schematron.
        check_nav_occurrence(&pkg, &opf_dir, &mut zip, &encrypted, &mut report);
    }
    check_xhtml_hrefs_and_reachability(
        &pkg,
        &opf_dir,
        epub2,
        epub3,
        &mut zip,
        &encrypted,
        &zip_paths,
        &opf_path,
        &mut report,
    );
    check_content_conformance(
        &pkg,
        &opf_dir,
        epub2,
        epub3,
        &mut zip,
        &encrypted,
        &mut report,
    );
    check_css_references(
        &pkg,
        &opf_dir,
        epub3,
        &mut zip,
        &encrypted,
        &zip_paths,
        &mut report,
    );
    check_xml_conformance(&opf_text, &opf_path, &mut report);
    // RSC-028 for the package document (its bytes already decoded as UTF-8, so
    // this catches a spurious non-UTF-8 `encoding=` declaration on ASCII bytes).
    check_xml_encoding(
        opf_text.as_bytes(),
        &opf_path,
        "application/oebps-package+xml",
        &mut report,
    );
    // OPF-073: the package document's own DOCTYPE (external identifiers are
    // forbidden in EPUB 3). Other XML resources are covered by check_xml_resources.
    check_doctype_rules(
        &opf_text,
        &opf_path,
        "application/oebps-package+xml",
        epub2,
        epub3,
        &mut report,
    );
    check_xml_well_formedness(&opf_text, &opf_path, &mut report);
    // RSC-005 schema channel: OPF attribute-grammar violations (opf:-namespaced
    // attributes in EPUB 3; non-id/toc spine attributes in EPUB 2) and the
    // package document's `id` attributes.
    check_opf_schema(&opf_text, &opf_path, epub2, epub3, &mut report);
    check_xml_resources(
        &pkg,
        &opf_dir,
        epub2,
        epub3,
        &mut zip,
        &encrypted,
        &mut report,
    );
    check_ncx_identifier(&pkg, &opf_dir, &mut zip, &encrypted, &mut report);
    if epub2 || epub3 {
        check_ncx_schema(&pkg, &opf_dir, &mut zip, &encrypted, &mut report);
    }
    check_obfuscated_fonts(&pkg, &opf_dir, &mut zip, &mut report);
    check_image_headers(&pkg, &opf_dir, &mut zip, &encrypted, &mut report);
    if epub3 {
        check_nav_remote_links(&pkg, &opf_dir, &mut zip, &encrypted, &mut report);
    }

    report
}

/// The XML-resource checks that read a resource's bytes: character encoding
/// (RSC-028 / HTM-058 / RSC-027), then — for the resources that decode as UTF-8
/// — XML version (HTM-001), external entities (HTM-003), and the DOCTYPE rules
/// (HTM-004 / OPF-073). A non-UTF-8 resource still gets its encoding finding;
/// the text-based checks simply skip it (its real content is unreadable here).
fn check_xml_resources(
    pkg: &opf::Package,
    opf_dir: &str,
    epub2: bool,
    epub3: bool,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    encrypted: &HashSet<String>,
    report: &mut Report,
) {
    for item in &pkg.manifest {
        if !is_xml_based(&item.media_type) {
            continue;
        }
        let path = join_opf(opf_dir, &item.href);
        // A resource listed in `META-INF/encryption.xml` is never content-checked
        // (epubcheck reports RSC-004 and skips it): its stored bytes are
        // encrypted/obfuscated, so parsing them judges ciphertext.
        if encrypted.contains(&path) {
            continue;
        }
        let Ok(bytes) = read_bytes(zip, &path) else {
            continue;
        };
        check_xml_encoding(&bytes, &path, &item.media_type, report);
        // The text-based conformance/doctype checks need a clean UTF-8 decode
        // (they simply skip a resource that isn't UTF-8).
        if let Ok(text) = std::str::from_utf8(&bytes) {
            check_xml_conformance(text, &path, report);
            check_doctype_rules(text, &path, &item.media_type, epub2, epub3, report);
        }
        // Well-formedness runs on the lossy decode when the resource is (or
        // defaults to) UTF-8: a UTF-8-declared resource with invalid bytes is a
        // fatal parse error in epubcheck too, and keeping the bytes as U+FFFD
        // preserves trailing/embedded garbage that Xerces stops on. A resource
        // that *declares* a non-UTF-8 charset is the RSC-028 case, not RSC-016.
        let declares_utf8 = sniff_xml_encoding(&bytes).is_none_or(|enc| enc == "UTF-8");
        if declares_utf8 {
            check_xml_well_formedness(&String::from_utf8_lossy(&bytes), &path, report);
        }
    }
}

/// RSC-028 / HTM-058 / RSC-027: an XML resource must be UTF-8. UTF-16 is an
/// error in XHTML (HTM-058) and a warning elsewhere (RSC-027); any other
/// non-UTF-8 encoding (UCS-4, EBCDIC, a declared legacy charset, …) is RSC-028.
fn check_xml_encoding(buf: &[u8], path: &str, media_type: &str, report: &mut Report) {
    let Some(enc) = sniff_xml_encoding(buf) else {
        return; // no declaration / BOM → treated as UTF-8
    };
    if enc == "UTF-8" {
        return;
    }
    if enc == "UTF-16" {
        if is_xhtml(media_type) {
            report.push(Violation::new(
                Rule::XhtmlEncodingUtf16,
                path.to_string(),
                "XHTML documents must be encoded in UTF-8, but UTF-16 was detected",
            ));
        } else {
            report.push(Violation::new(
                Rule::XmlEncodingUtf16,
                path.to_string(),
                "XML document is encoded in UTF-16; it should be UTF-8",
            ));
        }
    } else {
        report.push(Violation::new(
            Rule::XmlEncodingNotUtf8,
            path.to_string(),
            format!("XML documents must be encoded in UTF-8, but {enc} was detected"),
        ));
    }
}

/// The declared/detected character encoding of an XML document (uppercased), or
/// `None` when none is declared — which the XML spec treats as UTF-8. A faithful
/// port of epubcheck's `XMLEncodingSniffer`: byte-order marks first, then the
/// `encoding="…"` pseudo-attribute in the document's leading ASCII run.
fn sniff_xml_encoding(buf: &[u8]) -> Option<String> {
    let buf = &buf[..buf.len().min(256)];
    if buf.len() < 4 {
        return None;
    }
    // UTF-16: BOM, or `<?` encoded big/little-endian.
    if buf.starts_with(&[0xFE, 0xFF])
        || buf.starts_with(&[0xFF, 0xFE])
        || buf.starts_with(&[0x00, 0x3C, 0x00, 0x3F])
        || buf.starts_with(&[0x3C, 0x00, 0x3F, 0x00])
    {
        return Some("UTF-16".to_string());
    }
    // UCS-4 (all byte orders, with/without BOM).
    const UCS4: [[u8; 4]; 8] = [
        [0, 0, 0xFE, 0xFF],
        [0xFF, 0xFE, 0, 0],
        [0, 0, 0xFF, 0xFE],
        [0xFE, 0xFF, 0, 0],
        [0, 0, 0, 0x3C],
        [0, 0, 0x3C, 0],
        [0, 0x3C, 0, 0],
        [0x3C, 0, 0, 0],
    ];
    if UCS4.iter().any(|m| buf.starts_with(m)) {
        return Some("UCS-4".to_string());
    }
    if buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Some("UTF-8".to_string());
    }
    if buf.starts_with(&[0x4C, 0x6F, 0xA7, 0x94]) {
        return Some("EBCDIC".to_string());
    }
    // ASCII-compatible: scan the leading ASCII run for `encoding="…"`.
    let ascii_len = buf.iter().take_while(|&&c| c != 0 && c <= 0x7F).count();
    let header = std::str::from_utf8(&buf[..ascii_len]).ok()?;
    let rest = header.get(header.find("encoding=")? + "encoding=".len()..)?;
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let after = &rest[1..]; // quote is one ASCII byte
    let end = after.find(quote)?;
    Some(after[..end].to_ascii_uppercase())
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

/// RSC-016 (FATAL): the document is not well-formed, or not namespace-well-formed.
///
/// A faithful *subset* of what epubcheck's XML parser (Xerces, namespace-aware)
/// rejects fatally: a syntax error (mismatched end tag, stray `<`, malformed
/// attribute), an element left unclosed at end of file, an unbound namespace
/// prefix on an element or attribute, a duplicate attribute, and content after
/// the root element. The undeclared/unterminated *general entity* class is not
/// covered here — quick-xml surfaces entity references as [`Event::GeneralRef`]
/// without resolving them, and epubcheck resolves named entities against the
/// DTD its DOCTYPE points at (the XHTML entity sets are catalogued), so a plain
/// "not one of the five predefined" check false-positives on every book that
/// uses `&nbsp;`/`&mdash;`/… under an XHTML DOCTYPE. Detecting it zero-FP needs
/// that DTD-catalog entity model; left as a recall gap until then.
///
/// Under-detection is a recall gap, never a false positive: every construct
/// flagged is one Xerces also fatals on. Namespace binding is tracked with a
/// scope stack of the prefixes declared (`xmlns` / `xmlns:p`) on each open
/// element; `xml` is always bound. Reports the first problem only, matching
/// epubcheck, which stops at the first fatal parse error.
fn check_xml_well_formedness(text: &str, path: &str, report: &mut Report) {
    let mut reader = Reader::from_str(text);
    reader.config_mut().check_end_names = true;
    // Prefixes declared on each currently-open element (a frame per open depth).
    let mut scopes: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut depth: i32 = 0;
    let mut root_closed = false;
    loop {
        match reader.read_event() {
            Err(e) => {
                report.push(malformed_xml(path, format!("XML is not well-formed: {e}")));
                return;
            }
            Ok(Event::Eof) => {
                if depth > 0 {
                    report.push(malformed_xml(
                        path,
                        "XML is not well-formed: an element is not closed before end of file"
                            .to_string(),
                    ));
                }
                return;
            }
            Ok(Event::Start(e)) => {
                if depth == 0 && root_closed {
                    report.push(trailing_content_xml(path));
                    return;
                }
                if let Some(v) = open_element_well_formed(&e, &mut scopes, path) {
                    report.push(v);
                    return;
                }
                depth += 1;
            }
            Ok(Event::Empty(e)) => {
                if depth == 0 && root_closed {
                    report.push(trailing_content_xml(path));
                    return;
                }
                if let Some(v) = open_element_well_formed(&e, &mut scopes, path) {
                    report.push(v);
                    return;
                }
                scopes.pop(); // an empty element closes immediately
                if depth == 0 {
                    root_closed = true;
                }
            }
            Ok(Event::End(_)) => {
                scopes.pop();
                depth -= 1;
                if depth <= 0 {
                    depth = 0;
                    root_closed = true;
                }
            }
            Ok(Event::Text(t)) => {
                let bytes: &[u8] = t.as_ref();
                if depth == 0 && root_closed && !bytes.iter().all(u8::is_ascii_whitespace) {
                    report.push(trailing_content_xml(path));
                    return;
                }
            }
            _ => {}
        }
    }
}

fn malformed_xml(path: &str, message: String) -> Violation {
    Violation::new(Rule::MalformedXml, path.to_string(), message)
}

fn trailing_content_xml(path: &str) -> Violation {
    malformed_xml(
        path,
        "XML is not well-formed: content is not allowed after the root element".to_string(),
    )
}

/// Push a namespace scope for `e`, checking its attributes for duplicates and
/// its element/attribute prefixes for binding. Returns the first RSC-016
/// violation, if any (leaving the scope pushed is harmless — the caller stops).
fn open_element_well_formed(
    e: &quick_xml::events::BytesStart,
    scopes: &mut Vec<Vec<Vec<u8>>>,
    path: &str,
) -> Option<Violation> {
    let mut declared: Vec<Vec<u8>> = Vec::new();
    let mut used: Vec<Vec<u8>> = Vec::new();
    for attr in e.attributes() {
        // A duplicate attribute (checks are on by default) or malformed
        // attribute syntax — both are fatal parse errors in Xerces.
        let attr = match attr {
            Ok(a) => a,
            Err(err) => {
                return Some(malformed_xml(
                    path,
                    format!("XML is not well-formed: {err}"),
                ));
            }
        };
        let key = attr.key.as_ref();
        if key == b"xmlns" {
            declared.push(Vec::new()); // the default namespace = empty prefix
        } else if let Some(p) = key.strip_prefix(b"xmlns:") {
            declared.push(p.to_vec());
        } else if let Some(i) = key.iter().position(|&c| c == b':')
            && &key[..i] != b"xml"
        {
            used.push(key[..i].to_vec());
        }
    }
    // The element's own prefix.
    let name = e.name();
    if let Some(i) = name.as_ref().iter().position(|&c| c == b':')
        && &name.as_ref()[..i] != b"xml"
    {
        used.push(name.as_ref()[..i].to_vec());
    }
    scopes.push(declared);
    for prefix in used {
        if !scopes
            .iter()
            .flatten()
            .any(|d| d.as_slice() == prefix.as_slice())
        {
            return Some(malformed_xml(
                path,
                format!(
                    "XML is not namespace-well-formed: the prefix {:?} is not bound to a namespace",
                    String::from_utf8_lossy(&prefix)
                ),
            ));
        }
    }
    None
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

/// The OPF (package) namespace. Attributes bound to it are never permitted in an
/// EPUB 3 package document — `package-30.rnc` declares every attribute in no
/// namespace (only `xml:lang` is namespaced) — whereas in EPUB 2 they are the
/// legacy `opf:role`/`opf:file-as`/`opf:scheme`/… metadata refinements.
const OPF_NS: &[u8] = b"http://www.idpf.org/2007/opf";

/// The XHTML namespace. EPUB 2 content documents are routed through
/// `ops20.nvdl`, which validates only elements bound to it (everything else is
/// `allow`ed unvalidated), so the content-document schema rules below are scoped
/// to it — both faithfully and conservatively.
const XHTML_NS: &[u8] = b"http://www.w3.org/1999/xhtml";

/// The EPUB 3 structural-semantics namespace, which `epub:type` belongs to. The
/// nav schematron resolves `epub:type` through it, so a nav document that binds
/// the `epub` prefix to any other URI declares no typed navs at all — which is
/// precisely the defect [`check_nav_occurrence`] reports.
const OPS_NS: &[u8] = b"http://www.idpf.org/2007/ops";

/// The SVG namespace, for the SVG-specific content rules.
const SVG_NS: &[u8] = b"http://www.w3.org/2000/svg";

/// **RSC-005** (schema channel) — OPF attribute grammar. A namespace-resolved,
/// faithful *subset* of the package RelaxNG grammars (`package-30.rnc` /
/// `opf20.rng`), covering the two attribute-grammar violations that dominate the
/// RSC-005 recall gap on the corpus, each keyed to the version where it applies:
///
/// - **EPUB 3** — any attribute resolving to the OPF namespace. The EPUB 3
///   package grammar permits no OPF-namespaced attribute anywhere, so
///   `opf:role`/`opf:file-as`/`opf:scheme` on `<dc:*>` (and any other
///   OPF-namespaced attribute) is a schema violation. Legal in EPUB 2, hence the
///   gate.
/// - **EPUB 2** — an attribute other than (no-namespace) `id`/`toc` on
///   `<spine>`. The EPUB 2 spine attlist is exactly `{id?, toc}`, so the EPUB-3
///   `page-progression-direction` and the Adobe `page-map` extension (both legal
///   or ignorable in EPUB 3) are schema violations here.
///
/// Both package grammars additionally type `id` as `xsd:ID`, so the package
/// document's `id` attributes go through the shared [`IdScan`] (lexical NCName +
/// per-document uniqueness) on the same walk.
///
/// Namespaces are resolved through an explicit prefix→URI scope stack (an
/// unprefixed attribute has *no* namespace, per XML — the element's default
/// namespace never applies to its attributes). Under-detection is a recall gap,
/// never a false positive: every attribute flagged is one the corresponding
/// vendored grammar rejects. Reported once per offending attribute (the
/// granularity epubcheck's RelaxNG channel uses). Well-formedness is checked
/// separately, so a parse error here just ends the scan.
fn check_opf_schema(text: &str, path: &str, epub2: bool, epub3: bool, report: &mut Report) {
    if !epub2 && !epub3 {
        return; // version-less/legacy packages are OPF-001, not schema-validated
    }
    let mut reader = Reader::from_str(text);
    // In-scope namespace bindings: a frame of (prefix, uri) per open element.
    // The default namespace binds the empty prefix; `xml` is implicitly bound.
    let mut scopes: Vec<Vec<(Vec<u8>, Vec<u8>)>> = Vec::new();
    let mut ids = IdScan::default();
    // One frame per open element that requires children: (element, child it
    // requires, seen). Both grammars spell these as `element X { Y+ }`, so an
    // `<X>` that closes without a `<Y>` is a schema error.
    let mut required_children: Vec<(Vec<u8>, &'static [u8], bool)> = Vec::new();
    let requires = |local: &[u8]| -> Option<&'static [u8]> {
        match local {
            b"guide" => Some(b"reference"),
            b"tours" => Some(b"tour"),
            b"tour" => Some(b"site"),
            _ => None,
        }
    };
    let open = |e: &quick_xml::events::BytesStart,
                required_children: &mut Vec<(Vec<u8>, &'static [u8], bool)>| {
        let name = e.name();
        let local = local_name(name.as_ref()).to_vec();
        if let Some((_, child, seen)) = required_children.last_mut()
            && *child == local
        {
            *seen = true;
        }
        requires(&local).map(|child| (local, child))
    };
    loop {
        match reader.read_event() {
            Err(_) | Ok(Event::Eof) => return,
            Ok(Event::Start(e)) => {
                scopes.push(xmlns_frame(&e));
                report_disallowed_opf_attrs(&e, &scopes, epub2, epub3, path, report);
                ids.visit(&e, true, path, report);
                let entered = open(&e, &mut required_children);
                required_children.push(match entered {
                    Some((local, child)) => (local, child, false),
                    None => (Vec::new(), b"", true),
                });
            }
            Ok(Event::Empty(e)) => {
                scopes.push(xmlns_frame(&e));
                report_disallowed_opf_attrs(&e, &scopes, epub2, epub3, path, report);
                ids.visit(&e, true, path, report);
                if let Some((local, child)) = open(&e, &mut required_children) {
                    report_incomplete_element(&local, child, path, report);
                }
                scopes.pop();
            }
            Ok(Event::End(_)) => {
                scopes.pop();
                if let Some((local, child, seen)) = required_children.pop()
                    && !seen
                {
                    report_incomplete_element(&local, child, path, report);
                }
            }
            _ => {}
        }
    }
}

/// `element X { Y+ }` violated — an OPF element that closed without the child
/// its grammar requires (`<guide>` without a `<reference>`, `<tours>` without a
/// `<tour>`, `<tour>` without a `<site>`).
fn report_incomplete_element(local: &[u8], child: &[u8], path: &str, report: &mut Report) {
    report.push(Violation::new(
        Rule::PackageIncomplete,
        path.to_string(),
        format!(
            "element {:?} incomplete; missing required element {:?}",
            String::from_utf8_lossy(local),
            String::from_utf8_lossy(child)
        ),
    ));
}

/// The (prefix, uri) namespace declarations on one element: `xmlns="…"` binds the
/// empty prefix, `xmlns:p="…"` binds `p`.
fn xmlns_frame(e: &quick_xml::events::BytesStart) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut frame = Vec::new();
    for attr in e.attributes().flatten() {
        let key = attr.key.as_ref();
        if key == b"xmlns" {
            frame.push((Vec::new(), attr.value.as_ref().to_vec()));
        } else if let Some(p) = key.strip_prefix(b"xmlns:") {
            frame.push((p.to_vec(), attr.value.as_ref().to_vec()));
        }
    }
    frame
}

/// Resolve an attribute name's namespace URI. An unprefixed attribute has no
/// namespace (the default namespace never applies to attributes); `xml:` is
/// always the XML namespace; any other prefix is looked up innermost-first in the
/// scope stack (unbound → `None`, which the well-formedness pass reports as
/// RSC-016).
fn attr_namespace(key: &[u8], scopes: &[Vec<(Vec<u8>, Vec<u8>)>]) -> Option<Vec<u8>> {
    let colon = key.iter().position(|&c| c == b':')?;
    resolve_prefix(&key[..colon], scopes)
}

/// Resolve an *element* name's namespace URI: the binding for its prefix, or the
/// innermost default (`xmlns="…"`) binding when it has none.
fn elem_namespace(key: &[u8], scopes: &[Vec<(Vec<u8>, Vec<u8>)>]) -> Option<Vec<u8>> {
    let prefix = match key.iter().position(|&c| c == b':') {
        Some(colon) => &key[..colon],
        None => b"".as_slice(),
    };
    resolve_prefix(prefix, scopes)
}

/// The URI a namespace prefix is bound to, looked up innermost-first. `xml` is
/// always bound; an unbound prefix is `None` (the well-formedness pass reports
/// that as RSC-016).
fn resolve_prefix(prefix: &[u8], scopes: &[Vec<(Vec<u8>, Vec<u8>)>]) -> Option<Vec<u8>> {
    if prefix == b"xml" {
        return Some(b"http://www.w3.org/XML/1998/namespace".to_vec());
    }
    scopes
        .iter()
        .rev()
        .flatten()
        .find(|(p, _)| p.as_slice() == prefix)
        .map(|(_, uri)| uri.clone())
}

/// **RSC-005** (schema channel) — the two `id`-attribute rules every EPUB schema
/// shares, accumulated over one document:
///
/// - **Lexical.** The package (`package-30.rnc` / `opf20.rng`), NCX (`ncx.rng`)
///   and *EPUB 2* XHTML (`content.rng`) grammars all type `id` as `xsd:ID`, whose
///   lexical space is `xsd:NCName` — Jing reports a violation as "value of
///   attribute "id" is invalid; must be an XML name without colons". EPUB 3
///   content documents instead use the HTML5 `token` type, which allows anything
///   non-empty and whitespace-free, hence the per-document `ncname` flag.
/// - **Uniqueness.** `id-unique.sch` (EPUB 2 and EPUB 3 content documents, the
///   EPUB 3 package document) and the `*_idAttrUnique` patterns (the EPUB 2
///   package document, the NCX) all require an `id` value to occur once per
///   document.
///
/// Reported once per offending value so a document that repeats one id hundreds
/// of times yields one finding.
#[derive(Default)]
struct IdScan {
    seen: HashSet<String>,
    reported: HashSet<String>,
}

impl IdScan {
    fn visit(
        &mut self,
        e: &quick_xml::events::BytesStart,
        ncname: bool,
        path: &str,
        report: &mut Report,
    ) {
        let Some(attr) = e.attributes().flatten().find(|a| a.key.as_ref() == b"id") else {
            return;
        };
        let value = String::from_utf8_lossy(&attr.value).into_owned();
        if ncname && !may_be_ncname(&value) {
            report.push(Violation::new(
                Rule::IdNotNcName,
                path.to_string(),
                format!(
                    "value of attribute id ({value:?}) is invalid; must be an XML name without colons"
                ),
            ));
        }
        if !self.seen.insert(value.clone()) && self.reported.insert(value.clone()) {
            report.push(Violation::new(
                Rule::DuplicateId,
                path.to_string(),
                format!("duplicate id {value:?}"),
            ));
        }
    }
}

/// Whether `s` *may* be an `xsd:NCName` — deliberately one-sided. XML's `Name`
/// production admits a large set of Unicode letters whose exact membership varies
/// by XML edition, so only ASCII is judged: a value is rejected when it is empty,
/// starts with an ASCII character other than a letter or `_`, or contains an
/// ASCII character outside `[A-Za-z0-9._-]` (a colon, an interior space, `/`, …).
/// A value carrying non-ASCII characters is accepted. Every rejection is
/// therefore one the NCName production rejects in every edition —
/// under-detection is a recall gap, never a false positive.
///
/// `xsd:NCName` derives from `xsd:token`, whose `whiteSpace` facet is *collapse*,
/// so surrounding whitespace is stripped before the lexical space is checked:
/// `id=" x "` is valid (epubcheck's own
/// `conformance-xml-id-leading-trailing-spaces-valid` fixture).
fn may_be_ncname(s: &str) -> bool {
    let s = s.trim();
    let Some(first) = s.chars().next() else {
        return false; // xsd:NCName has no empty value
    };
    if first.is_ascii() && !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    !s.chars()
        .any(|c| c.is_ascii() && !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
}

/// The Dublin Core element namespace, and the XML Schema instance namespace that
/// carries `xsi:type` in EPUB 2 metadata.
const DC_NS: &[u8] = b"http://purl.org/dc/elements/1.1/";
const XSI_NS: &[u8] = b"http://www.w3.org/2001/XMLSchema-instance";
const XML_NS: &[u8] = b"http://www.w3.org/XML/1998/namespace";

fn report_disallowed_opf_attrs(
    e: &quick_xml::events::BytesStart,
    scopes: &[Vec<(Vec<u8>, Vec<u8>)>],
    epub2: bool,
    epub3: bool,
    path: &str,
    report: &mut Report,
) {
    let name = e.name();
    let local = local_name(name.as_ref());
    // The EPUB 2 element attlists this check knows, as the *union* of what any
    // one of them permits (`opf20.rng`) — an attribute outside the union is one
    // every relevant attlist rejects, so a union is zero-false-positive:
    //
    // - `<spine>`: `{id?, toc}`.
    // - `<item>`: `{id, href, media-type, fallback?, fallback-style?,
    //   required-namespace?, required-modules?}` — no EPUB 3 `properties`.
    // - `<dc:*>`: `{id?, xml:lang?, opf:role?, opf:file-as?, opf:scheme?,
    //   opf:event?, xsi:type?}` — the OPF refinements are *namespaced*, so an
    //   unprefixed `role`/`file-as`/`scheme` is a violation.
    let is_dc = elem_namespace(name.as_ref(), scopes).as_deref() == Some(DC_NS);
    let epub2_allowed: Option<(&str, &[&[u8]])> = match local {
        b"spine" => Some(("<spine>", &[b"id", b"toc"])),
        b"item" => Some((
            "<item>",
            &[
                b"id",
                b"href",
                b"media-type",
                b"fallback",
                b"fallback-style",
                b"required-namespace",
                b"required-modules",
            ],
        )),
        _ if is_dc => Some(("a <dc:*> element", &[b"id"])),
        _ => None,
    };
    for attr in e.attributes().flatten() {
        let key = attr.key.as_ref();
        if key == b"xmlns" || key.starts_with(b"xmlns:") {
            continue; // namespace declarations are not schema attributes
        }
        let ns = attr_namespace(key, scopes);
        if epub3 {
            // `package-30.rnc`: `opf.spine.attlist &= opf.id.attr? &
            // opf.spine.toc.attr? & opf.ppd.attr?` — nothing else, so the Adobe
            // `page-map` extension is a violation here as it is in EPUB 2.
            let bad_spine = local == b"spine"
                && ns.is_none()
                && !matches!(key, b"id" | b"toc" | b"page-progression-direction");
            if ns.as_deref() == Some(OPF_NS) || bad_spine {
                report.push(Violation::new(
                    Rule::DisallowedOpfAttribute,
                    path.to_string(),
                    format!(
                        "attribute {:?} is not permitted by the EPUB 3 package grammar",
                        String::from_utf8_lossy(key)
                    ),
                ));
            }
            continue;
        }
        let Some((element, allowed)) = epub2_allowed else {
            continue;
        };
        if !epub2 {
            continue;
        }
        let permitted = match ns.as_deref() {
            None => allowed.contains(&key),
            // A `<dc:*>` element takes the namespaced OPF refinements plus
            // `xml:lang` / `xsi:type`; no other element here takes a namespaced
            // attribute at all.
            Some(OPF_NS) => {
                is_dc && matches!(local_name(key), b"role" | b"file-as" | b"scheme" | b"event")
            }
            Some(XML_NS) => is_dc && local_name(key) == b"lang",
            Some(XSI_NS) => is_dc && local_name(key) == b"type",
            Some(_) => false,
        };
        if !permitted {
            report.push(Violation::new(
                Rule::DisallowedOpfAttribute,
                path.to_string(),
                format!(
                    "attribute {:?} is not permitted on {element} in an EPUB 2 package",
                    String::from_utf8_lossy(key)
                ),
            ));
        }
    }
}

/// **RSC-005** (schema channel) — the content-document schema rules that need no
/// grammar engine, over one XHTML content document:
///
/// - **`id` attributes** (both versions) — [`IdScan`]. The NCName constraint is
///   EPUB 2 only (EPUB 3 types `id` as an HTML5 token); uniqueness holds in both.
/// - **`<img>` required attributes** (EPUB 2) — XHTML 1.1's `img.attlist` makes
///   `alt` and `src` mandatory. EPUB 3's HTML5 grammar does not, so this is
///   version-gated (a missing `alt` there is an accessibility warning instead).
/// - **`<meta>` attributes** (EPUB 2) — XHTML 1.1's `meta.attlist` is exactly
///   `{http-equiv?, name?, content?, scheme?}` plus the i18n attributes, so the
///   HTML5 `charset` (and vendor extensions like Adobe's `value`) are violations.
/// - **Nested hyperlinks** (EPUB 2) — `20/sch/xhtml.sch` reports an `<a>` with an
///   `<a>` descendant.
///
/// Every rule is scoped to elements resolving to the XHTML namespace, matching
/// what `ops20.nvdl` routes into the grammar (other namespaces are allowed
/// through unvalidated). A parse error just ends the scan — well-formedness is
/// [`check_xml_well_formedness`]'s job.
fn check_content_schema(text: &str, path: &str, epub2: bool, epub3: bool, report: &mut Report) {
    if !epub2 && !epub3 {
        return; // version-less/legacy packages are OPF-001, not schema-validated
    }
    let mut reader = Reader::from_str(text);
    let mut scopes: Vec<Vec<(Vec<u8>, Vec<u8>)>> = Vec::new();
    // One frame per open element: whether it is an XHTML `<a>`, so a nested
    // hyperlink is "an `<a>` opened while another is still open".
    let mut anchors: Vec<bool> = Vec::new();
    let mut ids = IdScan::default();
    loop {
        match reader.read_event() {
            Err(_) | Ok(Event::Eof) => return,
            Ok(Event::Start(e)) => {
                scopes.push(xmlns_frame(&e));
                let in_anchor = anchors.iter().any(|a| *a);
                let is_anchor =
                    content_element_rules(&e, &scopes, epub2, in_anchor, &mut ids, path, report);
                anchors.push(is_anchor);
            }
            Ok(Event::Empty(e)) => {
                scopes.push(xmlns_frame(&e));
                let in_anchor = anchors.iter().any(|a| *a);
                content_element_rules(&e, &scopes, epub2, in_anchor, &mut ids, path, report);
                scopes.pop();
            }
            Ok(Event::End(_)) => {
                scopes.pop();
                anchors.pop();
            }
            _ => {}
        }
    }
}

/// `Common.attrib` of the vendored `20/rng/xhtml/attribs.rng` — `Core.attrib`
/// (`id`, `class`, `title`, `style`) plus `I18n.attrib` (`lang`, `xml:lang`,
/// `dir`). Almost every XHTML 1.1 element's attlist is this set plus a few of
/// its own. The event-handler module is not among `content.rng`'s includes, so
/// `onclick` and friends are declared nowhere.
const XHTML11_COMMON_ATTRS: &[&[u8]] = &[
    b"class",
    b"dir",
    b"id",
    b"lang",
    b"style",
    b"title",
    b"xml:lang",
];

/// The element and attribute grammar of an EPUB 2 XHTML content document,
/// derived from the vendored `20/rng/content-xhtml.rng` and the XHTML 1.1
/// modules it includes. Each entry is `(element, inherits_common, own)`:
/// `true` means the attlist is [`XHTML11_COMMON_ATTRS`] plus `own`, `false`
/// that `own` *is* the whole attlist (a dozen elements take only part of
/// `Common`).
///
/// The element list is exhaustive, which makes it two checks in one: an
/// XHTML-namespace element absent from it is declared nowhere in the schema.
/// The forms, ruby, frames, legacy and name-identification modules are not
/// included, so `<u>`, `<font>`, `<center>`, `<input>`, `<name>` and every
/// HTML5 element are errors in an EPUB 2 content document.
const XHTML11_ELEMENTS: &[Xhtml11Attlist] = &[
    (
        b"a",
        true,
        &[
            b"accesskey",
            b"charset",
            b"coords",
            b"href",
            b"hreflang",
            b"rel",
            b"rev",
            b"shape",
            b"tabindex",
            b"target",
            b"type",
        ],
    ),
    (b"abbr", true, &[]),
    (b"acronym", true, &[]),
    (b"address", true, &[]),
    (
        b"area",
        true,
        &[
            b"accesskey",
            b"alt",
            b"coords",
            b"href",
            b"nohref",
            b"shape",
            b"tabindex",
            b"target",
        ],
    ),
    (b"b", true, &[]),
    (b"bdo", true, &[]),
    (b"big", true, &[]),
    (b"blockquote", true, &[b"cite"]),
    (b"body", true, &[]),
    (b"caption", true, &[]),
    (b"cite", true, &[]),
    (b"code", true, &[]),
    (
        b"col",
        true,
        &[b"align", b"char", b"charoff", b"span", b"valign", b"width"],
    ),
    (
        b"colgroup",
        true,
        &[b"align", b"char", b"charoff", b"span", b"valign", b"width"],
    ),
    (b"dd", true, &[]),
    (b"del", true, &[b"cite", b"datetime"]),
    (b"dfn", true, &[]),
    (b"div", true, &[]),
    (b"dl", true, &[]),
    (b"dt", true, &[]),
    (b"em", true, &[]),
    (b"h1", true, &[]),
    (b"h2", true, &[]),
    (b"h3", true, &[]),
    (b"h4", true, &[]),
    (b"h5", true, &[]),
    (b"h6", true, &[]),
    (b"hr", true, &[]),
    (b"i", true, &[]),
    (
        b"img",
        true,
        &[
            b"alt",
            b"height",
            b"ismap",
            b"longdesc",
            b"src",
            b"usemap",
            b"width",
        ],
    ),
    (b"ins", true, &[b"cite", b"datetime"]),
    (b"kbd", true, &[]),
    (b"li", true, &[]),
    (
        b"link",
        true,
        &[
            b"charset",
            b"href",
            b"hreflang",
            b"media",
            b"rel",
            b"rev",
            b"type",
        ],
    ),
    (b"noscript", true, &[]),
    (
        b"object",
        true,
        &[
            b"archive",
            b"classid",
            b"codebase",
            b"codetype",
            b"data",
            b"declare",
            b"height",
            b"name",
            b"standby",
            b"tabindex",
            b"type",
            b"usemap",
            b"width",
        ],
    ),
    (b"ol", true, &[]),
    (b"p", true, &[]),
    (b"pre", true, &[b"xml:space"]),
    (b"q", true, &[b"cite"]),
    (b"samp", true, &[]),
    (b"small", true, &[]),
    (b"span", true, &[]),
    (b"strong", true, &[]),
    (b"sub", true, &[]),
    (b"sup", true, &[]),
    (
        b"table",
        true,
        &[
            b"border",
            b"cellpadding",
            b"cellspacing",
            b"frame",
            b"rules",
            b"summary",
            b"width",
        ],
    ),
    (b"tbody", true, &[b"align", b"char", b"charoff", b"valign"]),
    (
        b"td",
        true,
        &[
            b"abbr", b"align", b"axis", b"char", b"charoff", b"colspan", b"headers", b"rowspan",
            b"scope", b"valign",
        ],
    ),
    (b"tfoot", true, &[b"align", b"char", b"charoff", b"valign"]),
    (
        b"th",
        true,
        &[
            b"abbr", b"align", b"axis", b"char", b"charoff", b"colspan", b"headers", b"rowspan",
            b"scope", b"valign",
        ],
    ),
    (b"thead", true, &[b"align", b"char", b"charoff", b"valign"]),
    (b"tr", true, &[b"align", b"char", b"charoff", b"valign"]),
    (b"tt", true, &[]),
    (b"ul", true, &[]),
    (b"var", true, &[]),
    (
        b"applet",
        false,
        &[
            b"alt",
            b"archive",
            b"class",
            b"code",
            b"codebase",
            b"height",
            b"id",
            b"object",
            b"style",
            b"title",
            b"width",
        ],
    ),
    (b"base", false, &[b"href", b"target"]),
    (b"br", false, &[b"class", b"id", b"style", b"title"]),
    (b"head", false, &[b"dir", b"lang", b"profile", b"xml:lang"]),
    (b"html", false, &[b"dir", b"lang", b"version", b"xml:lang"]),
    (
        b"iframe",
        false,
        &[
            b"class",
            b"frameborder",
            b"height",
            b"id",
            b"longdesc",
            b"marginheight",
            b"marginwidth",
            b"scrolling",
            b"src",
            b"style",
            b"title",
            b"width",
        ],
    ),
    (
        b"map",
        false,
        &[b"class", b"dir", b"id", b"lang", b"title", b"xml:lang"],
    ),
    (
        b"meta",
        false,
        &[
            b"content",
            b"dir",
            b"http-equiv",
            b"lang",
            b"name",
            b"scheme",
            b"xml:lang",
        ],
    ),
    (
        b"param",
        false,
        &[b"id", b"name", b"type", b"value", b"valuetype"],
    ),
    (
        b"script",
        false,
        &[b"charset", b"defer", b"src", b"type", b"xml:space"],
    ),
    (
        b"style",
        false,
        &[
            b"dir",
            b"lang",
            b"media",
            b"title",
            b"type",
            b"xml:lang",
            b"xml:space",
        ],
    ),
    (b"title", false, &[b"dir", b"lang", b"xml:lang"]),
];

/// One row of [`XHTML11_ELEMENTS`]: `(element, inherits_common, own_attributes)`.
type Xhtml11Attlist = (&'static [u8], bool, &'static [&'static [u8]]);

/// The XHTML 1.1 attlist of one element: `(inherits_common, own_attributes)`,
/// or `None` when the schema declares no such element.
fn xhtml11_attlist(local: &[u8]) -> Option<(bool, &'static [&'static [u8]])> {
    XHTML11_ELEMENTS
        .iter()
        .find(|(name, _, _)| *name == local)
        .map(|(_, common, own)| (*common, *own))
}

/// True when `attr` is permitted on an element with this attlist.
fn xhtml11_allows(attlist: (bool, &[&[u8]]), attr: &[u8]) -> bool {
    let (inherits_common, own) = attlist;
    own.contains(&attr) || (inherits_common && XHTML11_COMMON_ATTRS.contains(&attr))
}

/// One element of [`check_content_schema`]'s walk. Returns whether the element is
/// an XHTML `<a>`, which the caller stacks to detect nested hyperlinks.
fn content_element_rules(
    e: &quick_xml::events::BytesStart,
    scopes: &[Vec<(Vec<u8>, Vec<u8>)>],
    epub2: bool,
    in_anchor: bool,
    ids: &mut IdScan,
    path: &str,
    report: &mut Report,
) -> bool {
    let ns = elem_namespace(e.name().as_ref(), scopes);
    if ns.as_deref() == Some(SVG_NS) {
        check_svg_element(e, path, report);
        return false;
    }
    if ns.as_deref() != Some(XHTML_NS) {
        return false;
    }
    ids.visit(e, epub2, path, report);
    let name = e.name();
    let local = local_name(name.as_ref());
    if !epub2 {
        check_epub3_content_element(e, local, path, report);
        return local == b"a";
    }
    report_disallowed_content_attrs(e, scopes, local, path, report);
    // XHTML 1.1 types `href`/`src` as `xsd:anyURI`, whose lexical space Jing
    // rejects for the same empty-host URLs the WHATWG parser does (RSC-020's
    // second class). EPUB 3's HTML5 grammar types them as plain strings.
    for attr in e.attributes().flatten() {
        if matches!(local_name(attr.key.as_ref()), b"href" | b"src")
            && url_has_empty_host(&String::from_utf8_lossy(&attr.value))
        {
            report.push(Violation::new(
                Rule::UriAttributeInvalid,
                path.to_string(),
                format!(
                    "value of attribute {:?} is invalid; must be a URI",
                    String::from_utf8_lossy(attr.key.as_ref())
                ),
            ));
        }
    }
    match local {
        b"img" => {
            for required in [b"alt".as_slice(), b"src".as_slice()] {
                if !e.attributes().flatten().any(|a| a.key.as_ref() == required) {
                    report.push(Violation::new(
                        Rule::MissingRequiredAttribute,
                        path.to_string(),
                        format!(
                            "element img missing required attribute {:?}",
                            String::from_utf8_lossy(required)
                        ),
                    ));
                }
            }
        }
        b"a" if in_anchor => {
            report.push(Violation::new(
                Rule::NestedHyperlink,
                path.to_string(),
                "the \"a\" element cannot contain any nested \"a\" elements",
            ));
        }
        _ => {}
    }
    local == b"a"
}

/// **RSC-015** — an SVG `<use>` instantiates the element its fragment names, so
/// a reference without one names nothing (epubcheck types it `SVG_SYMBOL` and
/// reports RSC-015 when the fragment is absent). Restricted to `<use>`, whose
/// href is meaningless without a fragment in every SVG version; the paint and
/// clip-path reference types epubcheck also covers need the SVG reference-typing
/// model that RSC-014 waits on, so missing them is a recall gap, not an FP.
fn check_svg_element(e: &quick_xml::events::BytesStart, path: &str, report: &mut Report) {
    if local_name(e.name().as_ref()) != b"use" {
        return;
    }
    // `href` (SVG 2) or `xlink:href` (SVG 1.1), by local name.
    let Some(href) = attr_by_local(e, b"href") else {
        return;
    };
    let href = href.trim();
    if !href.is_empty() && !href.contains('#') && !is_remote_href(href) {
        report.push(Violation::new(
            Rule::SvgReferenceNoFragment,
            path.to_string(),
            format!("a fragment identifier is required for the svg use reference {href:?}"),
        ));
    }
}

/// The two EPUB 3 content-document rules that are one element wide:
///
/// - `epub-xhtml-30.sch`'s `encoding.decl.state` — a `<meta>` whose `http-equiv`
///   is (case-insensitively) `content-type` must carry a `content` matching
///   `text/html;\s*charset=utf-8`. The Schematron uses an unanchored `matches()`
///   over `normalize-space(@content)`, and XSD's `\s` is ASCII-only, so a book
///   separating the parameters with an ideographic space fails it.
/// - `body.attrs` of the vendored `mod/html5/meta.rnc` — `<body>` accepts only
///   the four ARIA roles `application`, `document`, `none` and `presentation`
///   (a DPUB role such as `doc-cover` belongs on a descendant, not the body).
fn check_epub3_content_element(
    e: &quick_xml::events::BytesStart,
    local: &[u8],
    path: &str,
    report: &mut Report,
) {
    match local {
        b"meta" => {
            let Some(equiv) = attr_by_local(e, b"http-equiv") else {
                return;
            };
            if !equiv.trim().eq_ignore_ascii_case("content-type") {
                return;
            }
            let content = attr_by_local(e, b"content").unwrap_or_default();
            if !content_type_declares_utf8_html(&content) {
                report.push(Violation::new(
                    Rule::MetaEncodingDeclaration,
                    path.to_string(),
                    format!(
                        "the meta element in encoding declaration state (http-equiv=\"content-type\") must have the value \"text/html; charset=utf-8\", not {content:?}"
                    ),
                ));
            }
        }
        b"body" => {
            let Some(role) = attr_by_local(e, b"role") else {
                return;
            };
            const BODY_ROLES: [&str; 4] = ["application", "document", "none", "presentation"];
            if !BODY_ROLES.contains(&role.trim()) {
                report.push(Violation::new(
                    Rule::BodyRoleNotAllowed,
                    path.to_string(),
                    format!(
                        "value of attribute \"role\" is invalid on <body>; must be equal to \"application\", \"document\", \"none\" or \"presentation\", not {role:?}"
                    ),
                ));
            }
        }
        _ => {}
    }
}

/// The Schematron's `matches(normalize-space(@content),'text/html;\s*charset=utf-8','i')`
/// — an unanchored, ASCII-case-insensitive search whose only flexible part is a
/// run of ASCII whitespace after the semicolon.
fn content_type_declares_utf8_html(content: &str) -> bool {
    // `normalize-space` collapses only the four XML whitespace characters — an
    // ideographic space stays put and breaks the match, which is exactly the
    // corpus case.
    const XML_WS: [char; 4] = [' ', '\t', '\r', '\n'];
    let collapsed: String = content
        .split(XML_WS)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let hay = collapsed.to_ascii_lowercase();
    let Some(head) = hay.find("text/html;") else {
        return false;
    };
    let rest = hay[head + "text/html;".len()..].trim_start_matches([' ', '\t', '\n', '\r']);
    rest.starts_with("charset=utf-8")
}

/// The EPUB 2 content-document element and attribute grammar for one XHTML
/// element, read off [`XHTML11_ELEMENTS`]:
///
/// - An element the schema declares nowhere (`<u>`, `<center>`, `<input>`, any
///   HTML5 element) is an error in its own right, and its attributes are not
///   judged — epubcheck reports the element, not each attribute.
/// - Otherwise every no-namespace attribute must appear in the element's
///   attlist, as must `xml:lang` and `xml:space` (the only two the grammar
///   names in the XML namespace).
/// - An attribute in the EPUB 3 structural-semantics namespace (`epub:type`) is
///   declared nowhere, on any element.
///
/// Attributes in any *other* namespace are left alone, so this stays a subset of
/// what the grammar rejects: under-detection is a recall gap, never a false
/// positive.
fn report_disallowed_content_attrs(
    e: &quick_xml::events::BytesStart,
    scopes: &[Vec<(Vec<u8>, Vec<u8>)>],
    local: &[u8],
    path: &str,
    report: &mut Report,
) {
    let Some(attlist) = xhtml11_attlist(local) else {
        report.push(Violation::new(
            Rule::UndeclaredContentElement,
            path.to_string(),
            format!(
                "element {:?} is not allowed anywhere in an EPUB 2 content document",
                String::from_utf8_lossy(local)
            ),
        ));
        return;
    };
    for attr in e.attributes().flatten() {
        let key = attr.key.as_ref();
        if key == b"xmlns" || key.starts_with(b"xmlns:") {
            continue; // namespace declarations are not schema attributes
        }
        let ns = attr_namespace(key, scopes);
        let attr_local = local_name(key);
        // The grammar spells the two XML-namespace attributes it declares with
        // their `xml:` prefix; every other declared attribute is in no namespace.
        let judged: Option<Vec<u8>> = match ns.as_deref() {
            None => Some(attr_local.to_vec()),
            Some(XML_NS) if matches!(attr_local, b"lang" | b"space") => {
                Some([b"xml:", attr_local].concat())
            }
            _ => None,
        };
        let disallowed =
            ns.as_deref() == Some(OPS_NS) || judged.is_some_and(|k| !xhtml11_allows(attlist, &k));
        if disallowed {
            report.push(Violation::new(
                Rule::DisallowedContentAttribute,
                path.to_string(),
                format!(
                    "attribute {:?} is not permitted on <{}> in an EPUB 2 content document",
                    String::from_utf8_lossy(key),
                    String::from_utf8_lossy(local)
                ),
            ));
        }
    }
}

/// OPF package/manifest/spine structural rules that need no zip access beyond
/// the parsed [`opf::Package`] (epubcheck OPF-030/031/033/034/040/048/050/074/
/// 091/096/099). Resource *existence* is covered separately by
/// [`check_manifest_files`]; these check the package's internal consistency.
fn check_opf_structure(
    pkg: &opf::Package,
    opf_dir: &str,
    opf_path: &str,
    epub2: bool,
    epub3: bool,
    report: &mut Report,
) {
    let manifest_ids: HashSet<&str> = pkg.manifest.iter().map(|m| m.id.as_str()).collect();

    // OPF-048 / OPF-030: the package must declare a unique-identifier that
    // resolves to a <dc:identifier id="…">. The identifier-resolution half
    // (OPF-030) keys on bokai's modern `<dc:identifier id=…>` capture, which both
    // EPUB 2.0.1 and EPUB 3 use — so it fires for either, but not for a
    // version-less/OEB-1.x package (rejected via OPF-001; its legacy
    // `<dc-metadata>` structure would otherwise false-fire). The missing-attribute
    // half (OPF-048) is universal.
    match pkg.unique_identifier.as_deref() {
        None | Some("") => report.push(Violation::new(
            Rule::UniqueIdentifierMissing,
            opf_path,
            "package element is missing its required unique-identifier attribute",
        )),
        Some(uid) if (epub2 || epub3) && !pkg.identifier_ids.iter().any(|id| id == uid) => {
            report.push(Violation::new(
                Rule::UniqueIdentifierNotFound,
                opf_path,
                format!("unique-identifier {uid:?} matches no <dc:identifier id=…>"),
            ));
            // `package-30.sch`'s `opf.uid` asserts the same thing, so epubcheck
            // reports the schema channel alongside OPF-030. EPUB 2 instead types
            // the attribute as `xsd:IDREF` in `opf20.rng`, whose failure Jing
            // words differently — hence the EPUB 3 gate.
            if epub3 {
                report.push(Violation::new(
                    Rule::UnresolvedUniqueIdentifier,
                    opf_path,
                    format!(
                        "package element unique-identifier attribute does not resolve to a dc:identifier element (given reference was {uid:?})"
                    ),
                ));
            }
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

    // OPF-031 / OPF-032: every <guide><reference href> must be a declared
    // manifest item (OPF-031) *and* that item must be a content document
    // (OPF-032 — a guide reference to an image, stylesheet, … is invalid).
    let manifest_by_path: HashMap<String, &str> = pkg
        .manifest
        .iter()
        .map(|m| (join_opf(opf_dir, &m.href), m.media_type.as_str()))
        .collect();
    for href in &pkg.guide_hrefs {
        let file = href.split('#').next().unwrap_or(href);
        if file.is_empty() || file.contains("://") {
            continue;
        }
        let resolved = join_opf(opf_dir, file);
        match manifest_by_path.get(resolved.as_str()) {
            None => report.push(Violation::new(
                Rule::GuideReferenceNotInManifest,
                opf_path,
                format!("guide reference {href:?} is not declared in the manifest"),
            )),
            Some(mt) if !is_content_document(mt, epub2) => report.push(Violation::new(
                Rule::GuideReferenceNotContentDoc,
                opf_path,
                format!(
                    "guide reference {href:?} points at media-type {mt:?}, not a content document"
                ),
            )),
            _ => {}
        }
    }
}

/// Manifest fallback-chain integrity and spine-item fallback requirements:
///
/// - **OPF-045** — a cycle in the `fallback` graph (reported once).
/// - **OPF-043** — a spine item whose media type is not a spine-blessed content
///   type and which has no `fallback` at all.
/// - **OPF-044** — a spine item whose media type is not spine-blessed and whose
///   fallback chain never reaches a content document.
///
/// (Missing-target fallbacks are OPF-040, handled in [`check_opf_structure`].)
fn check_fallback_chain_and_spine(
    pkg: &opf::Package,
    epub2: bool,
    epub3: bool,
    opf_path: &str,
    report: &mut Report,
) {
    let by_id: HashMap<&str, &opf::ManifestItem> =
        pkg.manifest.iter().map(|m| (m.id.as_str(), m)).collect();

    // OPF-041: an EPUB 2 `fallback-style` must name a declared item (epubcheck's
    // `FallbackChainResolver` reads the attribute for version 2 only — the EPUB 3
    // manifest grammar has no `fallback-style`).
    for item in pkg.manifest.iter().filter(|_| epub2) {
        if let Some(style) = item.fallback_style.as_deref()
            && !by_id.contains_key(style)
        {
            report.push(Violation::new(
                Rule::FallbackStyleNotFound,
                opf_path,
                format!("fallback-style item with id {style:?} could not be found"),
            ));
        }
    }

    // OPF-045: first cycle in the fallback graph (edges to existing ids only —
    // a dangling fallback is OPF-040). Reported once, like epubcheck.
    'outer: for item in &pkg.manifest {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut cur = item.id.as_str();
        loop {
            if !seen.insert(cur) {
                report.push(Violation::new(
                    Rule::FallbackChainCircular,
                    opf_path,
                    format!("circular reference in the fallback chain at manifest id {cur:?}"),
                ));
                break 'outer;
            }
            match by_id.get(cur).and_then(|it| it.fallback.as_deref()) {
                Some(fb) if by_id.contains_key(fb) => cur = fb,
                _ => break,
            }
        }
    }

    // OPF-043 / OPF-044: a spine item with a non-blessed media type needs a
    // fallback (chain) that reaches a content document. EPUB 3-specific (the
    // spine content-model + fallback machinery is EPUB 3's; legacy packages use
    // a different model epubcheck rejects with OPF-001), so gate to EPUB 3.
    for s in pkg.spine.iter().filter(|_| epub3) {
        let Some(item) = by_id.get(s.idref.as_str()) else {
            continue; // unknown idref is OPF-049
        };
        if is_blessed_spine_type(&item.media_type, epub2) {
            continue;
        }
        if item.fallback.is_none() {
            report.push(Violation::new(
                Rule::SpineItemNoFallback,
                opf_path,
                format!(
                    "spine item id={:?} has non-standard media-type {:?} and no fallback",
                    item.id, item.media_type
                ),
            ));
        } else if !reaches_content_document(item, &by_id, epub2) {
            report.push(Violation::new(
                Rule::SpineItemFallbackNotContentDoc,
                opf_path,
                format!(
                    "spine item id={:?} (media-type {:?}) has no content-document fallback",
                    item.id, item.media_type
                ),
            ));
        }
    }
}

/// A media type permitted directly in the spine (epubcheck `isBlessedItemType`):
/// XHTML in either version, plus SVG (EPUB 3) or DTBook (EPUB 2). Deliberately
/// excludes the deprecated `text/html` / `text/x-oeb1-document` — those are
/// content documents for the guide (OPF-032) but not blessed spine items.
fn is_blessed_spine_type(media_type: &str, epub2: bool) -> bool {
    let mt = media_type.trim();
    mt.eq_ignore_ascii_case("application/xhtml+xml")
        || if epub2 {
            mt.eq_ignore_ascii_case("application/x-dtbook+xml")
        } else {
            mt.eq_ignore_ascii_case("image/svg+xml")
        }
}

/// epubcheck's `isDeprecatedBlessedItemType`: legacy content-document types that
/// still count as content documents for the hyperlink-target check (RSC-010).
fn is_deprecated_blessed_item_type(media_type: &str) -> bool {
    let mt = media_type.trim();
    mt.eq_ignore_ascii_case("text/html") || mt.eq_ignore_ascii_case("text/x-oeb1-document")
}

/// The blessed EPUB 3 image types (`OPFChecker.isBlessedImageType`) — the only
/// media types an `<img>`/`<source>` child of a `<picture>` may reference (MED-003
/// / MED-007).
fn is_blessed_image_type(media_type: &str) -> bool {
    matches!(
        media_type.trim(),
        "image/gif" | "image/png" | "image/jpeg" | "image/svg+xml" | "image/webp"
    )
}

/// The blessed EPUB 3 audio types (`OPFChecker30.isBlessedAudioType`).
fn is_blessed_audio_type(media_type: &str) -> bool {
    let mt = media_type.trim();
    mt == "audio/mpeg" || mt == "audio/mp4" || {
        // audio/ogg ; codecs=opus (optional whitespace around the `;`)
        let compact: String = mt.chars().filter(|c| !c.is_whitespace()).collect();
        compact == "audio/ogg;codecs=opus"
    }
}

/// An EPUB 3 core media type (`OPFChecker30.isCoreMediaType`) — a resource a
/// content document may reference directly, without a fallback.
fn is_core_media_type(media_type: &str) -> bool {
    let mt = media_type.trim();
    is_blessed_audio_type(mt)
        || mt.starts_with("video/")
        || is_blessed_font_type(mt)
        || mt == "application/xhtml+xml"
        || mt == "image/svg+xml"
        || is_blessed_image_type(mt)
        || matches!(
            mt,
            "text/javascript" | "application/javascript" | "application/ecmascript"
        )
        || mt == "text/css"
        || mt == "application/pls+xml"
        || mt == "application/smil+xml"
}

/// True when following `start`'s manifest `fallback` chain reaches a core-media-
/// type item (`Resource.hasCoreMediaTypeFallback`) — a foreign resource with such
/// a chain needs no intrinsic fallback (RSC-032).
fn reaches_core_media_type(
    start: &opf::ManifestItem,
    by_id: &HashMap<&str, &opf::ManifestItem>,
) -> bool {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut cur = start.fallback.as_deref();
    while let Some(fb) = cur {
        if !seen.insert(fb) {
            break; // cycle
        }
        let Some(item) = by_id.get(fb) else {
            break;
        };
        if is_core_media_type(&item.media_type) {
            return true;
        }
        cur = item.fallback.as_deref();
    }
    false
}

/// The RSC-010/011 verdict for a hyperlink whose target is the declared, present,
/// non-CFI manifest item `target` — a direct port of epubcheck's `HYPERLINK` case
/// in `ResourceReferencesChecker::checkReferenceType`:
/// - **RSC-010** if the target is not a content document (blessed or deprecated-
///   blessed) and its fallback chain reaches none either.
/// - else **RSC-011** if the target is not a spine item.
///
/// `None` when the hyperlink is conformant.
fn hyperlink_target_rule(
    target: &opf::ManifestItem,
    by_id: &HashMap<&str, &opf::ManifestItem>,
    spine_ids: &HashSet<&str>,
    epub2: bool,
) -> Option<Rule> {
    let mt = target.media_type.as_str();
    let blessed = is_blessed_spine_type(mt, epub2) || is_deprecated_blessed_item_type(mt);
    if !blessed && !reaches_content_document(target, by_id, epub2) {
        Some(Rule::HyperlinkToNonContentDocument)
    } else if !spine_ids.contains(target.id.as_str()) {
        Some(Rule::HyperlinkToNonSpineItem)
    } else {
        None
    }
}

/// True when `start`'s fallback chain reaches a spine-blessed content document
/// (cycle-guarded). Mirrors epubcheck's `hasContentDocumentFallback`.
fn reaches_content_document(
    start: &opf::ManifestItem,
    by_id: &HashMap<&str, &opf::ManifestItem>,
    epub2: bool,
) -> bool {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut cur = start.fallback.as_deref();
    while let Some(fb) = cur {
        if !seen.insert(fb) {
            break; // cycle → no content-document fallback found
        }
        let Some(item) = by_id.get(fb) else {
            break;
        };
        if is_blessed_spine_type(&item.media_type, epub2) {
            return true;
        }
        cur = item.fallback.as_deref();
    }
    false
}

/// The EPUB 3 reserved vocabulary prefixes — declared implicitly, so a property
/// token using one is never OPF-028. This is the **union** of every package
/// property context's reserved set (meta / item / itemref / link / link-rel);
/// epubcheck reserves a prefix per-context (e.g. `a11y` is reserved for `meta`
/// but not for `item`), so the union is a superset — using it means the port
/// never fires OPF-028 where epubcheck would stay silent (zero false positives),
/// at the cost of missing the rare cross-context case (a recall gap, not an FP).
const RESERVED_PREFIXES: &[&str] = &[
    "a11y",
    "dcterms",
    "marc",
    "media",
    "onix",
    "rendition",
    "schema",
    "xsd",
];

/// The prefixes declared in a package `prefix` attribute value (e.g.
/// `"foaf: http://xmlns.com/foaf/spec/ ex: http://example.org/"`). Deliberately
/// lenient — any whitespace token's leading `NCName:` segment counts as declared.
/// Over-capturing (e.g. reading a bare URI's `http:` scheme as a prefix) only
/// *suppresses* OPF-028, never introduces a false positive; the faithful state
/// machine's job here is just "which prefixes must not be flagged".
fn parse_declared_prefixes(decl: &str) -> HashSet<String> {
    let is_ncname_byte = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.');
    decl.split_whitespace()
        .filter_map(|tok| tok.split_once(':'))
        .map(|(prefix, _)| prefix)
        .filter(|prefix| !prefix.is_empty() && prefix.bytes().all(is_ncname_byte))
        .map(str::to_string)
        .collect()
}

/// OPF-028: every prefix used in a package-document property token
/// (`<meta property>`/`scheme`, `<item>`/`<itemref properties>`, `<link>`
/// `rel`/`properties`) must be reserved or declared in the package `prefix`
/// attribute. A token like `access:scroll-both` whose `access:` prefix is
/// neither is undeclared. EPUB 3 only — the prefix mechanism is EPUB 3's
/// (epubcheck runs it from `OPFHandler30`).
///
/// A bare/unprefixed token uses the default vocabulary (never OPF-028); a
/// malformed `:name`/`prefix:` token is OPF-026 (not ported); a token whose
/// prefix *is* known but whose local name is undefined is OPF-027 (not ported —
/// it needs the per-vocab name tables). Reported once per distinct prefix.
fn check_prefix_declarations(pkg: &opf::Package, opf_path: &str, report: &mut Report) {
    let declared = parse_declared_prefixes(pkg.prefix_decl.as_deref().unwrap_or(""));
    let mut reported: HashSet<String> = HashSet::new();
    for token in &pkg.property_tokens {
        // Split at the first colon (epubcheck's `([^:]*):(.*)`): a non-empty
        // prefix with a non-empty name is a prefixed property. `prefix:`/`:name`
        // are malformed (OPF-026); an unprefixed token has no colon.
        let Some((prefix, name)) = token.split_once(':') else {
            continue;
        };
        if prefix.is_empty() || name.is_empty() {
            continue;
        }
        if RESERVED_PREFIXES.contains(&prefix) || declared.contains(prefix) {
            continue;
        }
        if reported.insert(prefix.to_string()) {
            report.push(Violation::new(
                Rule::UndeclaredPrefix,
                opf_path.to_string(),
                format!(
                    "property prefix {prefix:?} is used but not declared; add it to the package `prefix` attribute"
                ),
            ));
        }
    }
}

/// RSC-020 for the URLs declared in the package document itself — manifest item
/// `href`s, `<guide>` references, and package `<link>` `href`s. (Content-document
/// references are checked in [`check_xhtml_hrefs_and_reachability`], where each is
/// already in hand.) See [`url_has_illegal_space`] for the ported class.
fn check_opf_url_validity(pkg: &opf::Package, opf_path: &str, report: &mut Report) {
    let hrefs = pkg
        .manifest
        .iter()
        .map(|m| m.href.as_str())
        .chain(pkg.guide_hrefs.iter().map(String::as_str))
        .chain(pkg.links.iter().map(|l| l.href.as_str()));
    for href in hrefs {
        if let Some(why) = invalid_url_reason(href) {
            report.push(Violation::new(
                Rule::InvalidUrl,
                opf_path.to_string(),
                format!("{href:?} is not a valid URL ({why})"),
            ));
        }
    }
}

/// Dublin Core metadata-value checks:
///
/// - **RSC-005** — the publication must declare at least one non-empty
///   `<dc:title>` and one `<dc:language>` (epubcheck enforces these through the
///   package schema; missing-required is version-independent).
/// - **OPF-053 / OPF-054** — every `<dc:date>` must be a valid W3C-DTF date
///   (warning in EPUB 3, error in EPUB 2).
/// - **OPF-085** — a `<dc:identifier>` marked as a UUID (a `urn:uuid:` value or
///   `opf:scheme="uuid"`) must be a syntactically valid UUID.
fn check_metadata(pkg: &opf::Package, epub2: bool, opf_path: &str, report: &mut Report) {
    let has = |name: &str| {
        pkg.metadata
            .iter()
            .any(|m| m.name == name && !m.value.is_empty())
    };
    if !has("title") {
        report.push(Violation::new(
            Rule::MissingTitle,
            opf_path.to_string(),
            "the package metadata must declare at least one non-empty <dc:title>",
        ));
    }
    if !has("language") {
        report.push(Violation::new(
            Rule::MissingLanguage,
            opf_path.to_string(),
            "the package metadata must declare at least one <dc:language>",
        ));
    }

    for m in &pkg.metadata {
        match m.name.as_str() {
            "language" => {
                // epubcheck checks only non-empty (trimmed) language tags; an
                // absent/empty dc:language is the schema's job (handled above).
                let tag = m.value.trim();
                if !tag.is_empty()
                    && let Err(detail) = language_tag_wellformed(tag)
                {
                    report.push(Violation::new(
                        Rule::LanguageTagNotWellFormed,
                        opf_path.to_string(),
                        format!("language tag {tag:?} is not well-formed: {detail}"),
                    ));
                }
            }
            "date" => {
                if let Err(detail) = parse_w3c_date(m.value.trim()) {
                    let rule = if epub2 {
                        Rule::DateNotValid
                    } else {
                        Rule::DateSyntaxNotRecommended
                    };
                    report.push(Violation::new(
                        rule,
                        opf_path.to_string(),
                        format!("date value {:?} is not valid W3C-DTF: {detail}", m.value),
                    ));
                }
            }
            "identifier" => {
                // Only identifiers that *claim* to be UUIDs are UUID-checked.
                let marked_uuid = m.value.starts_with("urn:uuid:")
                    || m.scheme
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case("uuid"));
                if marked_uuid {
                    let bare = m.value.replace("urn:uuid:", "");
                    if !is_valid_uuid(&bare) {
                        report.push(Violation::new(
                            Rule::IdentifierInvalidUuid,
                            opf_path.to_string(),
                            format!(
                                "dc:identifier value {:?} is marked as a UUID but is not one",
                                m.value
                            ),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
}

/// The fifteen Dublin Core elements the EPUB 3 package grammar declares. Every
/// one of them is typed `datatype.string.nonempty`, so an empty element is a
/// schema violation; the EPUB 2 grammar requires non-empty content only for
/// `<dc:identifier>` (`DC.metadata-required-content`).
const DC_ELEMENTS: &[&str] = &[
    "identifier",
    "title",
    "language",
    "date",
    "source",
    "type",
    "format",
    "creator",
    "subject",
    "description",
    "publisher",
    "contributor",
    "relation",
    "coverage",
    "rights",
];

/// **RSC-005** (schema channel) — Dublin Core element *content* requirements from
/// the package grammars: every EPUB 3 `<dc:*>` element (and an EPUB 2
/// `<dc:identifier>`) must have non-empty content, and the metadata must declare
/// at least one `<dc:identifier>`.
///
/// The missing-element half is gated to EPUB 3 like the sibling title/language
/// rules in [`check_metadata`]: a version-less OEB-1.x package nests its metadata
/// in `<dc-metadata>` and is rejected with OPF-001 instead.
fn check_dc_content(
    pkg: &opf::Package,
    epub2: bool,
    epub3: bool,
    opf_path: &str,
    report: &mut Report,
) {
    for m in &pkg.metadata {
        let required_nonempty = if epub3 {
            DC_ELEMENTS.contains(&m.name.as_str())
        } else {
            epub2 && m.name == "identifier"
        };
        if required_nonempty && m.value.trim().is_empty() {
            report.push(Violation::new(
                Rule::EmptyDcContent,
                opf_path.to_string(),
                format!(
                    "character content of element dc:{} is invalid; must be a string with length at least 1",
                    m.name
                ),
            ));
        }
    }
    if epub3 && !pkg.metadata.iter().any(|m| m.name == "identifier") {
        report.push(Violation::new(
            Rule::MissingIdentifier,
            opf_path.to_string(),
            "element metadata incomplete; missing required element dc:identifier",
        ));
    }
}

/// **RSC-005** (schema channel) — the package document's required structure,
/// where the two grammars agree that something must be present:
///
/// - `<manifest>` holds `opf.item+` / `oneOrMore item`, and `<spine>` holds
///   `opf.itemref+` / `oneOrMore itemref`, in both versions — so an absent or
///   empty manifest/spine is a violation either way.
/// - `<spine toc>` is **required** by the EPUB 2 grammar, and required by
///   `package-30.sch`'s `opf.toc.ncx.2` in EPUB 3 whenever an NCX is declared.
///   (Whether the pointer resolves to the NCX is OPF-050, checked separately.)
fn check_package_schema_structure(
    pkg: &opf::Package,
    epub2: bool,
    epub3: bool,
    opf_path: &str,
    report: &mut Report,
) {
    if !epub2 && !epub3 {
        return; // version-less/legacy packages are OPF-001, not schema-validated
    }
    for (empty, element, child) in [
        (pkg.manifest.is_empty(), "manifest", "item"),
        (pkg.spine.is_empty(), "spine", "itemref"),
    ] {
        if empty {
            report.push(Violation::new(
                Rule::PackageIncomplete,
                opf_path.to_string(),
                format!("element {element} incomplete; missing required element {child}"),
            ));
        }
    }
    let has_ncx = pkg.manifest.iter().any(|m| {
        m.media_type
            .eq_ignore_ascii_case("application/x-dtbncx+xml")
    });
    if pkg.spine_toc.is_none() && !pkg.spine.is_empty() && (epub2 || has_ncx) {
        report.push(Violation::new(
            Rule::SpineTocMissing,
            opf_path.to_string(),
            "spine element toc attribute must be set when an NCX is included in the publication",
        ));
    }
}

/// Canonical UUID syntax: `8-4-4-4-12` hex groups (case-insensitive). Mirrors
/// what `java.util.UUID.fromString` accepts for epubcheck's OPF-085 check,
/// after `urn:uuid:` has been stripped.
fn is_valid_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    let lengths = [8, 4, 4, 4, 12];
    groups.len() == 5
        && groups
            .iter()
            .zip(lengths)
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// BCP-47 (RFC 5646) *well-formedness*, a faithful port of Java's
/// `sun.util.locale.LanguageTag.parse` — the oracle epubcheck's OPF-092 uses via
/// `new Locale.Builder().setLanguageTag(tag)`. This checks syntax only, never
/// registry validity: `english` and `zz` are well-formed (accepted), while
/// `en_US`, `en-`, and `toolongsubtag` are not. `Ok(())` when well-formed; the
/// `Err` mirrors Java's `IllformedLocaleException` message.
///
/// Java collapses the RFC subtag productions to length+charclass tests and, like
/// Java, we split on `-` only (so `_` is never a separator) keeping empty tokens
/// (so a leading/trailing/doubled `-` surfaces as an "Empty subtag"). Grandfathered
/// tags are mapped to well-formed replacements before parsing, so they pass.
fn language_tag_wellformed(tag: &str) -> Result<(), String> {
    if is_grandfathered_langtag(tag) {
        return Ok(());
    }
    let alpha = |s: &str, lo: usize, hi: usize| {
        let n = s.len();
        n >= lo && n <= hi && s.bytes().all(|b| b.is_ascii_alphabetic())
    };
    let digit = |s: &str, lo: usize, hi: usize| {
        let n = s.len();
        n >= lo && n <= hi && s.bytes().all(|b| b.is_ascii_digit())
    };
    let alnum = |s: &str, lo: usize, hi: usize| {
        let n = s.len();
        n >= lo && n <= hi && s.bytes().all(|b| b.is_ascii_alphanumeric())
    };
    // variant = 5*8alphanum / (DIGIT 3alphanum)
    let is_variant = |s: &str| {
        alnum(s, 5, 8)
            || (s.len() == 4
                && s.as_bytes()[0].is_ascii_digit()
                && s.bytes().all(|b| b.is_ascii_alphanumeric()))
    };
    // singleton = one ALPHA that is not 'x'/'X' (Java excludes digit singletons).
    let is_singleton = |s: &str| alpha(s, 1, 1) && !s.eq_ignore_ascii_case("x");
    let is_privateuse_prefix = |s: &str| alpha(s, 1, 1) && s.eq_ignore_ascii_case("x");

    let toks: Vec<&str> = tag.split('-').collect();
    let n = toks.len();
    let mut i = 0usize;

    // langtag = language ["-" script] ["-" region] *variant *extension [privateuse]
    if i < n && alpha(toks[i], 2, 8) {
        i += 1; // language (Java: 2*8ALPHA)
        // extlang: up to 3 of 3ALPHA
        let mut extlangs = 0;
        while i < n && extlangs < 3 && alpha(toks[i], 3, 3) {
            i += 1;
            extlangs += 1;
        }
        if i < n && alpha(toks[i], 4, 4) {
            i += 1; // script = 4ALPHA
        }
        if i < n && (alpha(toks[i], 2, 2) || digit(toks[i], 3, 3)) {
            i += 1; // region = 2ALPHA / 3DIGIT
        }
        while i < n && is_variant(toks[i]) {
            i += 1;
        }
        // extension = singleton 1*("-" 2*8alphanum)
        while i < n && is_singleton(toks[i]) {
            let singleton = toks[i];
            i += 1;
            let sub_start = i;
            while i < n && alnum(toks[i], 2, 8) {
                i += 1;
            }
            if i == sub_start {
                return Err(format!("Incomplete extension '{singleton}'"));
            }
        }
    }
    // privateuse = ("x" / "X") 1*("-" 1*8alphanum)
    if i < n && is_privateuse_prefix(toks[i]) {
        i += 1;
        let sub_start = i;
        while i < n && alnum(toks[i], 1, 8) {
            i += 1;
        }
        if i == sub_start {
            return Err("Incomplete privateuse".into());
        }
    }

    if i < n {
        return Err(if toks[i].is_empty() {
            "Empty subtag".into()
        } else {
            format!("Invalid subtag: {}", toks[i])
        });
    }
    Ok(())
}

/// The RFC 5646 grandfathered tags (irregular + regular). Java's parser maps each
/// to a well-formed replacement before parsing, so all are well-formed; several
/// irregular ones (`i-klingon`, `en-GB-oed`, `sgn-*`) would otherwise fail the
/// `langtag` production. Matched case-insensitively, like Java's lookup.
fn is_grandfathered_langtag(tag: &str) -> bool {
    const GRANDFATHERED: &[&str] = &[
        "art-lojban",
        "cel-gaulish",
        "en-GB-oed",
        "i-ami",
        "i-bnn",
        "i-default",
        "i-enochian",
        "i-hak",
        "i-klingon",
        "i-lux",
        "i-mingo",
        "i-navajo",
        "i-pwn",
        "i-tao",
        "i-tay",
        "i-tsu",
        "no-bok",
        "no-nyn",
        "sgn-BE-FR",
        "sgn-BE-NL",
        "sgn-CH-DE",
        "zh-guoyu",
        "zh-hakka",
        "zh-min",
        "zh-min-nan",
        "zh-xiang",
    ];
    GRANDFATHERED.iter().any(|g| g.eq_ignore_ascii_case(tag))
}

/// EPUB 3 remote/data-URL manifest rules (epubcheck's `OPFChecker30`). **RSC-029**:
/// a manifest `<item href>` that is a `data:` URL is not allowed. **RSC-006**: a
/// spine item may never be a remote resource — spine content is always a content
/// document, so the audio/video/font remote exemption never applies to it, and the
/// rule fires regardless of scripts (unlike the non-spine cases).
///
/// The broader RSC-006 surface — a remote reference *from* content, or an
/// unreferenced remote manifest item — needs the content-document reference graph
/// and script detection, so it stays deferred. Under-reporting here is safe;
/// over-reporting (flagging a script-retrieved remote resource epubcheck rates
/// USAGE) is not.
fn check_remote_and_data_urls(pkg: &opf::Package, opf_path: &str, report: &mut Report) {
    let spine_ids: HashSet<&str> = pkg.spine.iter().map(|s| s.idref.as_str()).collect();
    for item in &pkg.manifest {
        if is_data_url(&item.href) {
            report.push(Violation::new(
                Rule::DataUrlNotAllowed,
                opf_path.to_string(),
                format!(
                    "manifest item {:?} uses a data: URL, which is not allowed",
                    item.id
                ),
            ));
        } else if is_remote_href(&item.href)
            && spine_ids.contains(item.id.as_str())
            && !is_remote_exempt_type(&item.media_type)
        {
            report.push(Violation::new(
                Rule::RemoteResourceInSpine,
                opf_path.to_string(),
                format!(
                    "spine item {:?} is a remote resource ({}); spine items must be in the container",
                    item.id, item.href
                ),
            ));
        }
    }
}

/// RSC-006: a remote reference from a content document is not allowed in EPUB 3
/// unless the context permits it. A hyperlink or non-stylesheet `<link>` may
/// always be remote; an audio, video, or font reference may be remote — either by
/// element type (`<audio>`/`<video>`/`<source>` in one) or because the declared
/// target's media type is audio/video/font; a spine item's own remoteness is
/// reported in the package document (`RemoteResourceInSpine`), so it's exempt
/// here. Every other remote reference — a remote image, stylesheet, object,
/// iframe, script, embed, track — is RSC-006. Direct port of
/// `ResourceReferencesChecker.checkRemoteReference`.
fn check_remote_references(
    pkg: &opf::Package,
    opf_dir: &str,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    encrypted: &HashSet<String>,
    report: &mut Report,
) {
    let spine_ids: HashSet<&str> = pkg.spine.iter().map(|s| s.idref.as_str()).collect();
    // Declared resources keyed by their href (remote URLs are absolute, so the
    // content reference and the manifest href are the same string): media type +
    // spine membership, for the audio/video/font and spine-item exemptions.
    let by_href: HashMap<&str, (&str, bool)> = pkg
        .manifest
        .iter()
        .map(|m| {
            (
                m.href.as_str(),
                (m.media_type.as_str(), spine_ids.contains(m.id.as_str())),
            )
        })
        .collect();
    for item in &pkg.manifest {
        if !is_xhtml(&item.media_type) {
            continue;
        }
        let path = join_opf(opf_dir, &item.href);
        // A resource listed in `META-INF/encryption.xml` is never content-checked
        // (epubcheck reports RSC-004 and skips it): its stored bytes are
        // encrypted/obfuscated, so parsing them judges ciphertext.
        if encrypted.contains(&path) {
            continue;
        }
        let Ok(text) = read_text(zip, &path) else {
            continue;
        };
        for (ty, href) in typed_content_refs(&text) {
            if !is_remote_href(&href) {
                continue;
            }
            let key = href.split('#').next().unwrap_or(&href);
            let exempt = matches!(ty, RefType::LinkLike | RefType::Audio | RefType::Video)
                || by_href
                    .get(key)
                    .is_some_and(|(mt, in_spine)| *in_spine || is_remote_exempt_type(mt));
            if !exempt {
                report.push(Violation::new(
                    Rule::RemoteResourceNotAllowed,
                    path.clone(),
                    format!(
                        "remote resource {href:?} is not allowed in this context; it must be located in the container"
                    ),
                ));
            }
        }
    }
}

/// One open element in the foreign-resource walk. Every non-void element pushes a
/// frame (so the stack stays balanced regardless of which elements bear
/// references), carrying its name, `hidden` flag, and accumulated palpable-content
/// state; `role` distinguishes the elements whose *end* triggers a check.
struct ForeignFrame {
    name: Vec<u8>,
    hidden: bool,
    palpable: bool,
    role: ForeignRole,
}

/// The reference-bearing role of a [`ForeignFrame`]. `Object` (`<object>`/
/// `<embed>`) resolves its fallback from palpable content at its end; `Media`
/// (`<audio>`/`<video>`) gathers `<source>` children (`href`, `type`) to decide
/// whether it has a core-type source; `Picture` marks the `<picture>` context.
enum ForeignRole {
    Object {
        href: Option<String>,
    },
    Media {
        sources: Vec<(String, Option<String>)>,
    },
    Picture,
    Plain,
}

/// epubcheck's `OPSHandler30.isPalpable`: whether an element counts as palpable
/// (fallback) content. A `hidden` element never does; embedded content always
/// does; document/metadata elements never do; every other element counts iff it
/// accumulated palpable content. (SVG/MathML are approximated by local name.)
fn is_palpable_elem(name: &[u8], hidden: bool, own_palpable: bool) -> bool {
    if hidden {
        return false;
    }
    match name {
        b"audio" | b"canvas" | b"embed" | b"iframe" | b"img" | b"object" | b"picture"
        | b"video" | b"svg" | b"math" => true,
        b"html" | b"head" | b"script" | b"link" | b"meta" | b"title" | b"style" => false,
        _ => own_palpable,
    }
}

/// RSC-032 / MED-003 / MED-007: fallback rules for foreign (non-core-media-type)
/// resources referenced from a content document — a port of
/// `ResourceReferencesChecker.checkFallbacks` and `OPSHandler30`'s media/image
/// handling.
///
/// - **RSC-032** — an `<img>`/`<object>`/`<embed>`/`<input>`/media reference whose
///   *declared* target is not a core media type must have a fallback: intrinsic (an
///   `<img>` in a `<picture>`, an `<object>`/`<embed>` with fallback content, a
///   media element with a core `<source>`) or a manifest `fallback` chain reaching
///   a core media type.
/// - **MED-003** — an `<img>` child of `<picture>` must reference a core image type.
/// - **MED-007** — a `<source>` child of `<picture>` with no `type` attribute must
///   reference a core image type.
///
/// Undeclared/missing targets are out of scope here (RSC-007/008 handle them).
fn check_foreign_resources(
    pkg: &opf::Package,
    opf_dir: &str,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    encrypted: &HashSet<String>,
    report: &mut Report,
) {
    let by_path: HashMap<String, &opf::ManifestItem> = pkg
        .manifest
        .iter()
        .map(|m| (join_opf(opf_dir, &m.href), m))
        .collect();
    let by_id: HashMap<&str, &opf::ManifestItem> =
        pkg.manifest.iter().map(|m| (m.id.as_str(), m)).collect();
    for item in &pkg.manifest {
        if !is_xhtml(&item.media_type) {
            continue;
        }
        let path = join_opf(opf_dir, &item.href);
        // A resource listed in `META-INF/encryption.xml` is never content-checked
        // (epubcheck reports RSC-004 and skips it): its stored bytes are
        // encrypted/obfuscated, so parsing them judges ciphertext.
        if encrypted.contains(&path) {
            continue;
        }
        let Ok(text) = read_text(zip, &path) else {
            continue;
        };
        walk_foreign_resources(&text, &path, &by_path, &by_id, report);
    }
}

/// Resolve a document-relative `href` to its declared manifest item.
fn foreign_target<'a>(
    path: &str,
    href: &str,
    by_path: &HashMap<String, &'a opf::ManifestItem>,
) -> Option<&'a opf::ManifestItem> {
    let resolved = resolve_href(path, href)?;
    by_path.get(&resolved).copied()
}

/// RSC-032 for one reference: a declared, non-core-media-type target with no
/// intrinsic and no manifest-chain fallback.
fn check_foreign_ref(
    path: &str,
    href: &str,
    intrinsic_fallback: bool,
    by_path: &HashMap<String, &opf::ManifestItem>,
    by_id: &HashMap<&str, &opf::ManifestItem>,
    report: &mut Report,
) {
    if intrinsic_fallback || is_remote_href(href) {
        return;
    }
    // A data: URL carries its own media type inline and has no manifest fallback.
    if is_data_url(href) {
        if let Some(mt) = data_url_media_type(href)
            && !is_core_media_type(mt)
        {
            report.push(Violation::new(
                Rule::ForeignResourceNoFallback,
                path.to_string(),
                format!(
                    "foreign data: resource (media-type {mt}) has no fallback to a core media type"
                ),
            ));
        }
        return;
    }
    if let Some(item) = foreign_target(path, href, by_path)
        && !is_core_media_type(&item.media_type)
        && !reaches_core_media_type(item, by_id)
    {
        report.push(Violation::new(
            Rule::ForeignResourceNoFallback,
            path.to_string(),
            format!(
                "foreign resource {href:?} (media-type {}) has no fallback to a core media type",
                item.media_type
            ),
        ));
    }
}

/// The media type of a `data:` URL — the text between `data:` and the first `;`
/// or `,` (`data:image/x-foo;base64,…` → `image/x-foo`). `None` when omitted.
fn data_url_media_type(href: &str) -> Option<&str> {
    let after = &href[href.find(':')? + 1..];
    let end = after.find([';', ',']).unwrap_or(after.len());
    let mt = after[..end].trim();
    (!mt.is_empty()).then_some(mt)
}

fn walk_foreign_resources(
    content: &str,
    path: &str,
    by_path: &HashMap<String, &opf::ManifestItem>,
    by_id: &HashMap<&str, &opf::ManifestItem>,
    report: &mut Report,
) {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(false);
    let mut stack: Vec<ForeignFrame> = Vec::new();
    let mut base = BaseUrl::default();
    loop {
        match reader.read_event() {
            Ok(ev @ (Event::Start(_) | Event::Empty(_))) => {
                let is_empty = matches!(ev, Event::Empty(_));
                let e = match &ev {
                    Event::Start(e) | Event::Empty(e) => e.clone(),
                    _ => unreachable!(),
                };
                base.visit(&e);
                let in_picture = stack.iter().any(|f| matches!(f.role, ForeignRole::Picture));
                // A media `<source>`'s parent is the immediately enclosing frame.
                let in_media = matches!(
                    stack.last().map(|f| &f.role),
                    Some(ForeignRole::Media { .. })
                );
                let name = e.name();
                let local = local_name(name.as_ref()).to_vec();
                let attr = |k: &[u8]| attr_by_local(&e, k);
                // A reference attribute is resolved against the base URL in scope
                // before it is matched against the manifest.
                let href_attr = |k: &[u8]| attr(k).map(|v| base.resolve(&v));
                let hidden = attr(b"hidden").is_some();
                // Emit this element's own references / picture checks.
                let mut role = ForeignRole::Plain;
                match local.as_slice() {
                    b"picture" => role = ForeignRole::Picture,
                    b"img" => {
                        if let Some(src) = href_attr(b"src") {
                            if in_picture {
                                // MED-003: a picture <img> must be a core image type.
                                if let Some(t) = foreign_target(path, &src, by_path)
                                    && !is_blessed_image_type(&t.media_type)
                                {
                                    report.push(Violation::new(
                                        Rule::PictureImgNotCoreType,
                                        path.to_string(),
                                        format!(
                                            "<picture> <img> {src:?} is media-type {}, not a core image type",
                                            t.media_type
                                        ),
                                    ));
                                }
                            } else {
                                check_foreign_ref(path, &src, false, by_path, by_id, report);
                            }
                        }
                    }
                    b"image" => {
                        // SVG <image> (href or xlink:href).
                        if let Some(src) = href_attr(b"href").or_else(|| href_attr(b"src")) {
                            check_foreign_ref(path, &src, in_picture, by_path, by_id, report);
                        }
                    }
                    b"math" => {
                        if let Some(alt) = href_attr(b"altimg") {
                            check_foreign_ref(path, &alt, false, by_path, by_id, report);
                        }
                    }
                    b"source" => {
                        // A media source is collected on its parent for the
                        // end-of-media fallback decision; a picture source is not
                        // subject to RSC-032 (an <img> sibling carries MED-003).
                        if in_media
                            && let Some(src) = href_attr(b"src")
                            && let Some(ForeignFrame {
                                role: ForeignRole::Media { sources },
                                ..
                            }) = stack.last_mut()
                        {
                            sources.push((src, attr(b"type")));
                        }
                    }
                    b"audio" | b"video" => {
                        // The media element's own `src` is registered with no
                        // intrinsic fallback; a <video> `poster` is an image.
                        if let Some(src) = href_attr(b"src") {
                            check_foreign_ref(path, &src, false, by_path, by_id, report);
                        }
                        if local.as_slice() == b"video"
                            && let Some(poster) = href_attr(b"poster")
                        {
                            check_foreign_ref(path, &poster, false, by_path, by_id, report);
                        }
                        role = ForeignRole::Media {
                            sources: Vec::new(),
                        };
                    }
                    b"object" | b"embed" => {
                        let href = if local.as_slice() == b"object" {
                            href_attr(b"data")
                        } else {
                            href_attr(b"src")
                        };
                        if is_empty {
                            // A void element can carry no fallback content.
                            if let Some(h) = href {
                                check_foreign_ref(path, &h, false, by_path, by_id, report);
                            }
                        } else {
                            role = ForeignRole::Object { href };
                        }
                    }
                    b"input" | b"iframe" => {
                        if let Some(src) = href_attr(b"src") {
                            check_foreign_ref(path, &src, false, by_path, by_id, report);
                        }
                    }
                    _ => {}
                }
                // Every non-void element pushes a frame so the stack stays
                // balanced; a void element bubbles its palpability to its parent.
                if is_empty {
                    if let Some(top) = stack.last_mut() {
                        top.palpable |= is_palpable_elem(&local, hidden, false);
                    }
                } else {
                    stack.push(ForeignFrame {
                        name: local,
                        hidden,
                        palpable: false,
                        role,
                    });
                }
            }
            Ok(Event::Text(t)) => {
                let bytes: &[u8] = t.as_ref();
                if !bytes.iter().all(|b| b.is_ascii_whitespace())
                    && let Some(top) = stack.last_mut()
                {
                    top.palpable = true;
                }
            }
            Ok(Event::End(_)) => {
                if let Some(f) = stack.pop() {
                    match &f.role {
                        ForeignRole::Object { href: Some(h) } => {
                            check_foreign_ref(path, h, f.palpable, by_path, by_id, report);
                        }
                        ForeignRole::Media { sources } => {
                            // Intrinsic fallback iff any source is a core media type
                            // (its `type` attribute if present, else the declared
                            // manifest media type).
                            let has_fallback = sources.iter().any(|(src, ty)| match ty {
                                Some(t) => is_core_media_type(remove_media_params(t)),
                                None => foreign_target(path, src, by_path)
                                    .is_some_and(|it| is_core_media_type(&it.media_type)),
                            });
                            for (src, _) in sources {
                                check_foreign_ref(path, src, has_fallback, by_path, by_id, report);
                            }
                        }
                        _ => {}
                    }
                    // Propagate this element's palpability to its parent.
                    let palpable = is_palpable_elem(&f.name, f.hidden, f.palpable);
                    if let Some(parent) = stack.last_mut() {
                        parent.palpable |= palpable;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
}

/// Strip the parameters from a media type (`audio/mpeg; codecs=…` → `audio/mpeg`).
fn remove_media_params(mt: &str) -> &str {
    mt.split(';').next().unwrap_or(mt).trim()
}

/// The lowercased scheme of an absolute URL reference (`"http"`, `"data"`,
/// `"file"`, …), or `None` for a relative reference. RFC 3986: a scheme is an
/// ALPHA followed by ALPHA/DIGIT/`+`/`-`/`.`, then `:`. A relative reference's
/// first path segment cannot form a scheme (a `:` there is preceded by a
/// non-scheme char or a `/`), so `../a`, `text/ch.xhtml`, and `#frag` yield `None`.
fn url_scheme(href: &str) -> Option<String> {
    let colon = href.find(':')?;
    let scheme = &href[..colon];
    let mut bytes = scheme.bytes();
    if !bytes.next()?.is_ascii_alphabetic() {
        return None;
    }
    bytes
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        .then(|| scheme.to_ascii_lowercase())
}

/// A `data:` URL (inline resource) — disallowed for manifest items (RSC-029).
fn is_data_url(href: &str) -> bool {
    url_scheme(href).as_deref() == Some("data")
}

/// RSC-020: a referenced URL that is not valid per the WHATWG URL standard.
/// Ported for the class the corpus exhibits — a raw *interior* space (U+0020) in
/// a URL's path or host, which galimatias (epubcheck's strict URL parser) always
/// rejects ("space is not allowed"); such a URL must percent-encode the space
/// (`%20`).
///
/// Faithful to the URL parser's preprocessing: leading/trailing C0-control-or-
/// space is stripped and all tab/newline removed *before* parsing, so those are
/// never errors (a trailing space like `"http://x "` is stripped, and epubcheck
/// stays silent — so we must too). Only the hierarchical part (before any
/// `?`/`#`) is judged, and `data:` URLs (whose whitespace epubcheck collapses)
/// are exempt. Other illegal-character classes are a recall gap here, not an FP.
fn url_has_illegal_space(href: &str) -> bool {
    if is_data_url(href) {
        return false;
    }
    // Strip leading/trailing C0-control-or-space, then drop interior tab/newline
    // (both removed by the URL parser without error), leaving only true interior
    // spaces to judge.
    let trimmed = href.trim_matches(|c: char| c <= ' ');
    let cleaned: String = trimmed
        .chars()
        .filter(|&c| c != '\t' && c != '\n' && c != '\r')
        .collect();
    cleaned
        .split(['?', '#'])
        .next()
        .unwrap_or(&cleaned)
        .contains(' ')
}

/// RSC-020, second corpus class: a *special*-scheme URL with an empty host.
/// The WHATWG URL parser requires a non-empty host for `http`/`https`/`ws`/
/// `wss`/`ftp` and fails with "host is missing", so `href="http://"` and
/// `http:///path` are invalid — while `file:///path` (whose host may legally be
/// empty) is not. The same emptiness makes the value fail XSD `anyURI` in the
/// EPUB 2 content grammar, which is why it also feeds the schema channel.
fn url_has_empty_host(href: &str) -> bool {
    let href = href.trim_matches(|c: char| c <= ' ');
    let Some(scheme) = url_scheme(href) else {
        return false;
    };
    if !matches!(scheme.as_str(), "http" | "https" | "ws" | "wss" | "ftp") {
        return false;
    }
    let Some(after) = href[scheme.len() + 1..].strip_prefix("//") else {
        return false; // no authority component at all — a different failure
    };
    after
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after)
        .is_empty()
}

/// Why a referenced URL is invalid (RSC-020), or `None` when it parses. The two
/// classes the corpus exhibits, both of which galimatias (epubcheck's strict URL
/// parser) always rejects: [`url_has_illegal_space`] and [`url_has_empty_host`].
fn invalid_url_reason(href: &str) -> Option<&'static str> {
    if url_has_illegal_space(href) {
        Some("a space must be percent-encoded as %20")
    } else if url_has_empty_host(href) {
        Some("host is missing")
    } else {
        None
    }
}

/// A remote reference: an absolute URL to another origin. `data:` is inline (its
/// own rule) and `file:` is disallowed separately (RSC-030); every other scheme
/// (http, https, ftp, …) denotes a remote origin outside the container.
fn is_remote_href(href: &str) -> bool {
    !matches!(
        url_scheme(href).as_deref(),
        None | Some("data") | Some("file")
    )
}

/// Media types epubcheck permits as remote resources in EPUB 3: audio, video,
/// fonts, and legacy Shockwave Flash.
fn is_remote_exempt_type(mt: &str) -> bool {
    mt.starts_with("audio/")
        || mt.starts_with("video/")
        || mt.starts_with("font/")
        || mt.starts_with("application/font-")
        || mt == "application/vnd.ms-opentype"
        || mt == "application/x-shockwave-flash"
}

/// EPUB 3 package `<link>` element rules (epubcheck's `OPFHandler30::processLink`
/// and `OPFChecker30::checkLinkedResources`):
/// - **RSC-029** — a `data:` URL on a `<link>` is not allowed.
/// - **OPF-098** — the `href` must not reference an element *inside the package
///   document itself* (a fragment resolving back to the OPF).
/// - **OPF-089** — the `alternate` rel keyword must not be paired with others.
/// - **OPF-095** — a `voicing` link must have an audio media type.
/// - **OPF-067** — a link must not resolve to a manifest item that is not a spine
///   item (a publication resource is referenced twice, once outside the spine).
///
/// Faithful to epubcheck's control flow: RSC-029 and OPF-098 short-circuit the
/// link (matching its `return`, which also skips registration, so OPF-067 cannot
/// then apply); OPF-089/095 do not.
fn check_opf_links(pkg: &opf::Package, opf_dir: &str, opf_path: &str, report: &mut Report) {
    let spine_ids: HashSet<&str> = pkg.spine.iter().map(|s| s.idref.as_str()).collect();
    for link in &pkg.links {
        let href = &link.href;
        // RSC-029: data: URL on a <link>.
        if is_data_url(href) {
            report.push(Violation::new(
                Rule::DataUrlNotAllowed,
                opf_path.to_string(),
                "a <link> element uses a data: URL, which is not allowed".to_string(),
            ));
            continue;
        }
        // OPF-098: a fragment referencing the package document itself.
        if let Some((file, frag)) = href.split_once('#')
            && !frag.is_empty()
        {
            let doc = if file.is_empty() {
                Some(opf_path.to_string()) // same-document "#frag"
            } else {
                resolve_href(opf_path, href) // resolves file part, strips fragment
            };
            if doc.as_deref() == Some(opf_path) {
                report.push(Violation::new(
                    Rule::LinkIntoPackageDocument,
                    opf_path.to_string(),
                    format!("<link href={href:?}> references an element in the package document"),
                ));
                continue;
            }
        }
        // OPF-089: 'alternate' paired with other rel keywords.
        if link.rel.iter().any(|r| r == "alternate") && link.rel.len() > 1 {
            report.push(Violation::new(
                Rule::LinkAlternatePaired,
                opf_path.to_string(),
                format!(
                    "<link rel> pairs \"alternate\" with other keywords: {:?}",
                    link.rel
                ),
            ));
        }
        // OPF-095: 'voicing' links require an audio media type.
        if link.rel.iter().any(|r| r == "voicing")
            && let Some(mt) = &link.media_type
            && !mt.starts_with("audio/")
        {
            report.push(Violation::new(
                Rule::VoicingLinkNotAudio,
                opf_path.to_string(),
                format!("<link rel=\"voicing\"> has media-type {mt:?}, not an audio type"),
            ));
        }
        // OPF-067: a *metadata* link resolving to a manifest item that is not in
        // the spine. Only metadata `<link>`s populate epubcheck's linked-resources
        // set; collection `<link>`s (preview/dictionary) are governed by the
        // collection rules instead, so they must not trigger OPF-067.
        if link.in_metadata
            && let Some(target) = resolve_href(opf_path, href)
        {
            for item in &pkg.manifest {
                if join_opf(opf_dir, &item.href) == target && !spine_ids.contains(item.id.as_str())
                {
                    report.push(Violation::new(
                        Rule::LinkToNonSpineManifestItem,
                        opf_path.to_string(),
                        format!(
                            "<link href={href:?}> points at manifest item {:?}, which is not in the spine",
                            item.id
                        ),
                    ));
                    break;
                }
            }
        }
    }
}

/// Validate a W3C-DTF (ISO-8601 profile) date, a faithful port of epubcheck's
/// `DateParser` plus its four-digit-year guard. `Ok(())` when valid; `Err`
/// carries a short reason. Accepts `YYYY`, `YYYY-MM`, `YYYY-MM-DD`, and the full
/// `YYYY-MM-DDThh:mm:ss(.s)?(Z|±hh:mm)` forms; rejects out-of-range fields.
fn parse_w3c_date(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("zero-length string".into());
    }
    let toks = tokenize_keeping_delims(s, "-T:.+Z");
    let mut i = 0;
    let next = |i: &mut usize| -> Option<&str> {
        let t = toks.get(*i).map(String::as_str);
        *i += 1;
        t
    };
    // Consume an expected delimiter and require more tokens after it. Returns
    // Ok(false) when the input ended cleanly (no more tokens).
    let expect = |i: &mut usize, delim: &str| -> Result<bool, String> {
        let Some(t) = toks.get(*i) else {
            return Ok(false);
        };
        *i += 1;
        if t != delim {
            return Err(format!("unexpected {t:?}"));
        }
        if *i >= toks.len() {
            return Err("incomplete date".into());
        }
        Ok(true)
    };
    let int = |t: &str| -> Result<i64, String> {
        t.parse().map_err(|_| format!("{t:?} is not an integer"))
    };

    // Year (required, and — per epubcheck's guard — at most four digits).
    let year_tok = next(&mut i).ok_or("empty date")?;
    let year = int(year_tok)?;
    if year_tok.len() > 4 || year < 0 {
        return Err(format!("{year_tok:?} is not a four-digit year"));
    }
    // Month.
    if !expect(&mut i, "-")? {
        return Ok(());
    }
    let month = int(next(&mut i).unwrap_or(""))?;
    if !(1..=12).contains(&month) {
        return Err(format!("month {month} out of range"));
    }
    // Day.
    if !expect(&mut i, "-")? {
        return Ok(());
    }
    let day = int(next(&mut i).unwrap_or(""))?;
    if day < 1 || day > days_in_month(year, month as u32) {
        return Err(format!("day {day} out of range"));
    }
    // Time.
    if !expect(&mut i, "T")? {
        return Ok(());
    }
    let hour = int(next(&mut i).unwrap_or(""))?;
    if !(0..=23).contains(&hour) {
        return Err(format!("hour {hour} out of range"));
    }
    if !expect(&mut i, ":")? {
        return Ok(());
    }
    let minute = int(next(&mut i).unwrap_or(""))?;
    if !(0..=59).contains(&minute) {
        return Err(format!("minute {minute} out of range"));
    }
    if i >= toks.len() {
        return Ok(());
    }
    // Seconds are optional; the next token is either ":" (seconds) or a zone.
    let mut tok = next(&mut i).unwrap_or("").to_string();
    if tok == ":" {
        let second = int(next(&mut i).ok_or("no seconds specified")?)?;
        if !(0..=59).contains(&second) {
            return Err(format!("second {second} out of range"));
        }
        if i >= toks.len() {
            return Ok(());
        }
        tok = next(&mut i).unwrap_or("").to_string();
        if tok == "." {
            // Fractional seconds: digits only.
            let frac = next(&mut i).ok_or("missing fraction")?;
            int(frac)?;
            if i >= toks.len() {
                return Ok(());
            }
            tok = next(&mut i).unwrap_or("").to_string();
        }
    }
    // Time zone: `Z`, or `±hh:mm`.
    if tok == "Z" {
        return if i >= toks.len() {
            Ok(())
        } else {
            Err("unexpected field after Z".into())
        };
    }
    if tok != "+" && tok != "-" {
        return Err(format!("expected Z, + or -, found {tok:?}"));
    }
    int(next(&mut i).ok_or("missing zone hour")?)?; // zone hour (not range-checked, per epubcheck)
    if !expect(&mut i, ":")? {
        return Err("missing zone minute".into());
    }
    int(next(&mut i).ok_or("missing zone minute")?)?;
    Ok(())
}

/// Split `s` into maximal non-delimiter runs and single-character delimiter
/// tokens, in order — the equivalent of Java's `StringTokenizer(s, delims, true)`.
fn tokenize_keeping_delims(s: &str, delims: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if delims.contains(c) {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            out.push(c.to_string());
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Days in `month` (1–12) of `year`, with the proleptic Gregorian leap rule.
fn days_in_month(year: i64, month: u32) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
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
    epub2: bool,
    epub3: bool,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    encrypted: &HashSet<String>,
    report: &mut Report,
) {
    let fxl_ids = fixed_layout_spine_ids(pkg);
    for item in &pkg.manifest {
        if !is_xhtml(&item.media_type) {
            continue;
        }
        let path = join_opf(opf_dir, &item.href);
        // A resource listed in `META-INF/encryption.xml` is never content-checked
        // (epubcheck reports RSC-004 and skips it): its stored bytes are
        // encrypted/obfuscated, so parsing them judges ciphertext.
        if encrypted.contains(&path) {
            continue;
        }
        let Ok(text) = read_text(zip, &path) else {
            continue;
        };
        // The schema-channel rules that hold in both versions (and the EPUB 2
        // content grammar) run first; everything below is EPUB 3 only.
        check_content_schema(&text, &path, epub2, epub3, report);
        // The non-empty-<title> requirement is EPUB 3 only — epubcheck enforces
        // it through the EPUB 3 XHTML content-document schema. An EPUB 2 book
        // (XHTML 1.1, DTD-validated, which can't require element text) is not
        // held to it, so firing there is a false positive.
        if !epub3 {
            continue;
        }
        // The DOCTYPE rule (HTM-004) is applied to every XML resource by
        // [`check_doctype_rules`] (via [`check_xml_resources`]); here we only
        // check the non-empty-<title> requirement (RSC-005).
        if has_empty_title(&text) {
            report.push(Violation::new(
                Rule::EmptyTitle,
                path.clone(),
                "content document has an empty <title>; EPUB 3 requires non-empty title text",
            ));
        }

        let features = detect_content_features(&text);
        // OPF-014: a manifest property the content *requires* (inline SVG /
        // MathML / scripting / a remote resource) must be declared on the item.
        // Detection is a subset of epubcheck's, so a missed feature is a recall
        // gap, never a false positive (see [`detect_content_features`]).
        for (required, property) in [
            (features.inline_svg, "svg"),
            (features.mathml, "mathml"),
            (features.scripted, "scripted"),
            (features.remote_resources, "remote-resources"),
        ] {
            if required && !item.properties.iter().any(|p| p == property) {
                report.push(Violation::new(
                    Rule::PropertyNotDeclared,
                    path.clone(),
                    format!(
                        "content document requires the {property:?} manifest property, not declared on its <item>"
                    ),
                ));
            }
        }
        // HTM-046: a fixed-layout content document must carry a viewport meta;
        // HTM-047/056/057/059 judge the first one's content. A reflowable
        // document's viewport is only USAGE (HTM-060b), so it is not parsed.
        if fxl_ids.contains(item.id.as_str()) {
            match features.viewport_content.as_deref() {
                Some(content) => check_viewport_meta(content, &path, report),
                None => report.push(Violation::new(
                    Rule::FixedLayoutNoViewport,
                    path.clone(),
                    "fixed-layout content document has no <meta name=\"viewport\"> element",
                )),
            }
        }
        // CSS-015: an alternate stylesheet must be named, so a reading system can
        // offer the choice.
        if features.untitled_alternate_stylesheet {
            report.push(Violation::new(
                Rule::AlternateStylesheetNoTitle,
                path.clone(),
                "alternative style sheets must have a title",
            ));
        }
    }
    // SVG content documents (EPUB 3 only — EPUB 2 has no SVG content document)
    // go through the same schema walk, plus the fixed-layout `viewBox` rule.
    for item in pkg.manifest.iter().filter(|_| epub3) {
        if !item.media_type.trim().eq_ignore_ascii_case("image/svg+xml") {
            continue;
        }
        let path = join_opf(opf_dir, &item.href);
        if encrypted.contains(&path) {
            continue;
        }
        let Ok(text) = read_text(zip, &path) else {
            continue;
        };
        check_content_schema(&text, &path, epub2, epub3, report);
        // HTM-048: a fixed-layout SVG needs a `viewBox` on its outermost `<svg>`
        // for the reading system to know its intrinsic dimensions.
        if fxl_ids.contains(item.id.as_str()) && !outermost_svg_has_viewbox(&text) {
            report.push(Violation::new(
                Rule::SvgFixedLayoutNoViewBox,
                path.clone(),
                "fixed-layout SVG document has no \"viewBox\" attribute on its outermost <svg>",
            ));
        }
    }
}

/// Whether the first `<svg>` element of a document carries a `viewBox`. `None`
/// of the document parses is treated as "has one" — a malformed SVG is
/// [`check_xml_well_formedness`]'s finding, not this rule's.
fn outermost_svg_has_viewbox(text: &str) -> bool {
    let mut reader = Reader::from_str(text);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e))
                if local_name(e.name().as_ref()) == b"svg" =>
            {
                return attr_by_local(&e, b"viewBox").is_some();
            }
            Err(_) | Ok(Event::Eof) => return true,
            _ => {}
        }
    }
}

/// The manifest ids of the spine items that are Fixed-Layout Documents, resolved
/// exactly as epubcheck's `OPFHandler30::processItemrefProperties`: an itemref
/// with `rendition:layout-pre-paginated` is fixed; one with
/// `rendition:layout-reflowable` is reflowable; otherwise it inherits the
/// publication default (`<meta property="rendition:layout">pre-paginated`).
/// Only spine items can be fixed-layout (a manifest item outside the spine has
/// no itemref to carry the override, and the global default does not reach it).
fn fixed_layout_spine_ids(pkg: &opf::Package) -> HashSet<&str> {
    let global_fxl = pkg.rendition_layout.as_deref() == Some("pre-paginated");
    pkg.spine
        .iter()
        .filter(|s| {
            let has = |name: &str| s.properties.iter().any(|p| p == name);
            has("rendition:layout-pre-paginated")
                || (!has("rendition:layout-reflowable") && global_fxl)
        })
        .map(|s| s.idref.as_str())
        .collect()
}

/// DOCTYPE-declaration conformance, keyed on the resource's media type and the
/// publication version (epubcheck `DeclarationHandler`):
///
/// - **XHTML** (EPUB 3 only): only `<!DOCTYPE html>` or `<!DOCTYPE html SYSTEM
///   "about:legacy-compat">` is allowed. A public identifier, or any other system
///   identifier, is **HTM-004**. EPUB 2 XHTML permits the XHTML 1.1 public DTD, so
///   the rule is version-gated to avoid false positives on 2.0 content.
/// - **Other XML** (EPUB 3 only): an external identifier (`PUBLIC`/`SYSTEM`) is
///   forbidden — **OPF-073** — except the three fixed DTDs epubcheck sanctions
///   for SVG, MathML, and NCX. EPUB 2 permits legacy DTDs, so the rule is
///   version-gated to avoid false positives on 2.0 content.
fn check_doctype_rules(
    text: &str,
    path: &str,
    media_type: &str,
    epub2: bool,
    epub3: bool,
    report: &mut Report,
) {
    let Some(dt) = parse_doctype(text) else {
        return; // no declaration → nothing to check (epubcheck only inspects one present)
    };
    if epub3 {
        if is_xhtml(media_type) {
            // EPUB 3 XHTML: only `<!DOCTYPE html>` / `about:legacy-compat`.
            let legacy_ok = dt.public_id.is_none()
                && dt
                    .system_id
                    .as_deref()
                    .is_none_or(|s| s == "about:legacy-compat");
            if !legacy_ok {
                report.push(Violation::new(
                    Rule::IrregularDoctype,
                    path.to_string(),
                    format!(
                        "irregular DOCTYPE {:?}; EPUB 3 requires `<!DOCTYPE html>`",
                        dt.raw
                    ),
                ));
            }
        } else if (dt.public_id.is_some() || dt.system_id.is_some())
            && !dt.is_allowed_dtd(media_type)
        {
            report.push(Violation::new(
                Rule::DoctypeExternalIdentifier,
                path.to_string(),
                format!(
                    "DOCTYPE {:?} declares an external identifier, not allowed in EPUB 3",
                    dt.raw
                ),
            ));
        }
    } else if epub2 {
        // EPUB 2 (OPS 2.0): the content DTD must be *exactly* the sanctioned one
        // — XHTML 1.1 for XHTML content documents, NCX 2005-1 for the NCX. Any
        // other declaration (an HTML5 `<!DOCTYPE html>`, XHTML 1.0, a bare/empty
        // public id, a mismatched system id, …) is HTM-004. Faithful port of
        // epubcheck's `DeclarationHandler` version-2 branch. A version-less/legacy
        // package is not EPUB 2 (it is rejected via OPF-001), so this needs a
        // positive EPUB 2 version.
        let expected: Option<(&str, &str)> = if is_xhtml(media_type) && dt.root == "html" {
            Some((
                "-//W3C//DTD XHTML 1.1//EN",
                "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd",
            ))
        } else if media_type
            .trim()
            .eq_ignore_ascii_case("application/x-dtbncx+xml")
        {
            Some((
                "-//NISO//DTD ncx 2005-1//EN",
                "http://www.daisy.org/z3986/2005/ncx-2005-1.dtd",
            ))
        } else {
            None
        };
        if let Some((pub_id, sys_id)) = expected
            && (dt.public_id.as_deref() != Some(pub_id) || dt.system_id.as_deref() != Some(sys_id))
        {
            report.push(Violation::new(
                Rule::IrregularDoctype,
                path.to_string(),
                format!(
                    "irregular DOCTYPE {:?}; EPUB 2 requires the {pub_id:?} DTD",
                    dt.raw
                ),
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

/// A parsed DOCTYPE declaration: the root name and its external identifiers.
struct Doctype {
    /// The full `<!DOCTYPE …>` text, for diagnostics.
    raw: String,
    /// The declared root element name (e.g. `html`, `ncx`, `package`).
    root: String,
    public_id: Option<String>,
    system_id: Option<String>,
}

impl Doctype {
    /// True when this DOCTYPE's `(public_id, system_id)` is exactly the DTD
    /// epubcheck sanctions for `media_type` (SVG 1.1, MathML 3.0, or NCX
    /// 2005-1). Both identifiers must match. Any other media type → false.
    fn is_allowed_dtd(&self, media_type: &str) -> bool {
        let mt = media_type.trim();
        let (pub_id, sys_id) = if mt.eq_ignore_ascii_case("image/svg+xml") {
            (
                "-//W3C//DTD SVG 1.1//EN",
                "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd",
            )
        } else if mt.eq_ignore_ascii_case("application/mathml+xml")
            || mt.eq_ignore_ascii_case("application/mathml-content+xml")
            || mt.eq_ignore_ascii_case("application/mathml-presentation+xml")
        {
            (
                "-//W3C//DTD MathML 3.0//EN",
                "http://www.w3.org/Math/DTD/mathml3/mathml3.dtd",
            )
        } else if mt.eq_ignore_ascii_case("application/x-dtbncx+xml") {
            (
                "-//NISO//DTD ncx 2005-1//EN",
                "http://www.daisy.org/z3986/2005/ncx-2005-1.dtd",
            )
        } else {
            return false;
        };
        self.public_id.as_deref() == Some(pub_id) && self.system_id.as_deref() == Some(sys_id)
    }
}

/// Parse the leading DOCTYPE into its external identifiers. Recognizes
/// `<!DOCTYPE root>`, `… SYSTEM "sys"`, and `… PUBLIC "pub" "sys"` (either
/// quote style). Returns `None` when the document has no DOCTYPE.
fn parse_doctype(s: &str) -> Option<Doctype> {
    let raw = find_doctype(s)?;
    // Inner text, between `<!DOCTYPE` (9 ASCII bytes) and the trailing `>`.
    let inner = raw.get(9..raw.len().saturating_sub(1)).unwrap_or("").trim();
    // `to_ascii_uppercase` preserves byte length, so keyword offsets map 1:1
    // back onto `inner` (the keyword itself is ASCII → a char boundary).
    let upper = inner.to_ascii_uppercase();
    let (mut public_id, mut system_id) = (None, None);
    if let Some(pos) = upper.find("PUBLIC") {
        let (first, rest) = take_quoted(&inner[pos + "PUBLIC".len()..]);
        public_id = first;
        system_id = take_quoted(rest).0;
    } else if let Some(pos) = upper.find("SYSTEM") {
        system_id = take_quoted(&inner[pos + "SYSTEM".len()..]).0;
    }
    let root = inner
        .split(|c: char| c.is_whitespace() || c == '[' || c == '>')
        .next()
        .unwrap_or("")
        .to_string();
    Some(Doctype {
        raw: raw.to_string(),
        root,
        public_id,
        system_id,
    })
}

/// Take the first `"…"`/`'…'`-quoted string from `s` (after any leading
/// whitespace). Returns `(value, remainder_after_closing_quote)`.
fn take_quoted(s: &str) -> (Option<String>, &str) {
    let s = s.trim_start();
    let quote = match s.chars().next() {
        Some(c @ ('"' | '\'')) => c,
        _ => return (None, s),
    };
    let rest = &s[1..]; // the quote is one ASCII byte
    match rest.find(quote) {
        Some(end) => (Some(rest[..end].to_string()), &rest[end + 1..]),
        None => (None, ""),
    }
}

/// True when the package declares an EPUB 2 `version` (`"2.0"`, `"2"`, …).
/// Absent or 3.x versions are treated as EPUB 3 (bokai's target), so
/// version-gated rules default to the stricter EPUB-3 behavior.
fn is_epub2(pkg: &opf::Package) -> bool {
    pkg.version
        .as_deref()
        .is_some_and(|v| v.trim().starts_with('2'))
}

/// True only when the package explicitly declares an EPUB 3 version. EPUB-3-
/// specific rules gate on *this* (not `!is_epub2`), so a version-less or legacy
/// (OEB 1.x / unparseable-version) package — which epubcheck rejects with OPF-001
/// rather than validating as EPUB 3 — is not falsely held to EPUB 3 requirements.
fn is_epub3(pkg: &opf::Package) -> bool {
    pkg.version
        .as_deref()
        .is_some_and(|v| v.trim().starts_with('3'))
}

/// A "content document" media type — epubcheck's blessed + deprecated-blessed
/// item types. Both versions accept XHTML and the legacy `text/x-oeb1-document`
/// / `text/html`; EPUB 3 adds SVG, EPUB 2 adds DTBook.
fn is_content_document(media_type: &str, epub2: bool) -> bool {
    let mt = media_type.trim();
    mt.eq_ignore_ascii_case("application/xhtml+xml")
        || mt.eq_ignore_ascii_case("text/x-oeb1-document")
        || mt.eq_ignore_ascii_case("text/html")
        || if epub2 {
            mt.eq_ignore_ascii_case("application/x-dtbook+xml")
        } else {
            mt.eq_ignore_ascii_case("image/svg+xml")
        }
}

/// NCX-001: when the publication carries both an NCX `dtb:uid` and an OPF
/// unique-identifier value, the two must be equal (the NCX uid is trimmed
/// before comparison, matching epubcheck's `NCXChecker`). Fires only when both
/// are present, so a book without an NCX — or without a resolvable
/// unique-identifier — is never flagged.
fn check_ncx_identifier(
    pkg: &opf::Package,
    opf_dir: &str,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    encrypted: &HashSet<String>,
    report: &mut Report,
) {
    let Some(opf_uid) = pkg.unique_identifier_value.as_deref() else {
        return;
    };
    let Some(ncx) = pkg.manifest.iter().find(|m| {
        m.media_type
            .eq_ignore_ascii_case("application/x-dtbncx+xml")
    }) else {
        return;
    };
    let path = join_opf(opf_dir, &ncx.href);
    // An encrypted/obfuscated resource is never content-checked (epubcheck
    // reports RSC-004 and skips it).
    if encrypted.contains(&path) {
        return;
    }
    let Ok(text) = read_text(zip, &path) else {
        return;
    };
    let Some(ncx_uid) = ncx_dtb_uid(&text) else {
        return;
    };
    if ncx_uid.trim() != opf_uid {
        report.push(Violation::new(
            Rule::NcxUidMismatch,
            path,
            format!(
                "NCX dtb:uid {:?} does not match the OPF unique identifier {opf_uid:?}",
                ncx_uid.trim()
            ),
        ));
    }
}

/// **RSC-005** (schema channel) — the NCX rules that need no grammar engine, from
/// `20/sch/ncx.sch` and `ncx.rng` (both applied to the NCX of an EPUB 2 *and* an
/// EPUB 3 publication — `OPFChecker.checkItemContent` builds an `NCXChecker`
/// regardless of version):
///
/// - **`id` attributes** — `ncx.rng` types them `xsd:ID` and `ncx_idAttrUnique`
///   requires uniqueness ([`IdScan`]).
/// - **`ncx_playOrderOrigin`** — if anything carries `playOrder`, some node must
///   carry `playOrder="1"` (a string comparison in the schematron, so `"01"` does
///   not satisfy it).
/// - **`ncx_playOrderNoGaps`** — for every `playOrder` value above 1, its
///   predecessor must exist.
/// - **`ncx_playOrderMatch2`** — nodes sharing a `playOrder` value must point at
///   the same target (`<content src>`); a node without a `<content>` child never
///   triggers it, matching the empty-node-set semantics of the XPath assertion.
///
/// Each violation is reported once per offending value rather than once per node,
/// so a mis-numbered NCX yields findings proportional to the defect.
fn check_ncx_schema(
    pkg: &opf::Package,
    opf_dir: &str,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    encrypted: &HashSet<String>,
    report: &mut Report,
) {
    let Some(ncx) = pkg.manifest.iter().find(|m| {
        m.media_type
            .eq_ignore_ascii_case("application/x-dtbncx+xml")
    }) else {
        return;
    };
    let path = join_opf(opf_dir, &ncx.href);
    // An encrypted/obfuscated resource is never content-checked (epubcheck
    // reports RSC-004 and skips it).
    if encrypted.contains(&path) {
        return;
    }
    let Ok(text) = read_text(zip, &path) else {
        return;
    };

    let mut reader = Reader::from_str(&text);
    let mut ids = IdScan::default();
    // One frame per open element, holding the index of the node it opened when
    // that element carries `playOrder` — so a `<content src>` can be attributed
    // to its *parent* node, as the schematron's `current()/ncx:content` requires.
    let mut open: Vec<Option<usize>> = Vec::new();
    // (playOrder attribute value, target of the node's `<content src>` child).
    let mut nodes: Vec<(String, Option<String>)> = Vec::new();
    let mut visit =
        |e: &quick_xml::events::BytesStart, open: &mut Vec<Option<usize>>, report: &mut Report| {
            ids.visit(e, true, &path, report);
            // `ncx.rng`: `navPoint` declares `id` as a required `xsd:ID`
            // attribute (unlike `navLabel`/`content`, which take none).
            if local_name(e.name().as_ref()) == b"navPoint" && attr_by_local(e, b"id").is_none() {
                report.push(Violation::new(
                    Rule::MissingRequiredAttribute,
                    path.clone(),
                    "element \"navPoint\" missing required attribute \"id\"",
                ));
            }
            if local_name(e.name().as_ref()) == b"content"
                && let Some(Some(idx)) = open.last().copied()
                && let Some(src) = attr_by_local(e, b"src")
            {
                nodes[idx].1 = Some(src);
            }
            match attr_by_local(e, b"playOrder") {
                Some(order) => {
                    nodes.push((order, None));
                    Some(nodes.len() - 1)
                }
                None => None,
            }
        };
    loop {
        match reader.read_event() {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let idx = visit(&e, &mut open, report);
                open.push(idx);
            }
            Ok(Event::Empty(e)) => {
                visit(&e, &mut open, report);
            }
            Ok(Event::End(_)) => {
                open.pop();
            }
            _ => {}
        }
    }
    check_ncx_play_order(&nodes, &path, report);
}

/// The three `playOrder` schematron assertions over one NCX's nodes, each node
/// given as its raw `playOrder` value and the `src` of its `<content>` child.
/// `number()` semantics: a non-numeric value is NaN, which never equals anything,
/// so such a node takes part in no comparison.
fn check_ncx_play_order(nodes: &[(String, Option<String>)], path: &str, report: &mut Report) {
    if nodes.is_empty() {
        return;
    }
    if !nodes.iter().any(|(order, _)| order == "1") {
        report.push(Violation::new(
            Rule::NcxPlayOrder,
            path.to_string(),
            "the first playOrder value is not 1",
        ));
    }
    let numeric: Vec<(f64, Option<&str>)> = nodes
        .iter()
        .filter_map(|(order, src)| {
            let n = order.trim().parse::<f64>().ok().filter(|n| n.is_finite())?;
            Some((n, src.as_deref()))
        })
        .collect();
    let mut reported_gap: HashSet<String> = HashSet::new();
    let mut reported_split: HashSet<String> = HashSet::new();
    for (order, src) in &numeric {
        if *order > 1.0 && !numeric.iter().any(|(other, _)| *other == *order - 1.0) {
            let key = format!("{order}");
            if reported_gap.insert(key) {
                report.push(Violation::new(
                    Rule::NcxPlayOrder,
                    path.to_string(),
                    format!("playOrder sequence has gaps (no node precedes playOrder {order})"),
                ));
            }
        }
        if let Some(src) = src
            && numeric
                .iter()
                .any(|(other, other_src)| *other == *order && other_src.is_some_and(|s| s != *src))
        {
            let key = format!("{order}");
            if reported_split.insert(key) {
                report.push(Violation::new(
                    Rule::NcxPlayOrder,
                    path.to_string(),
                    format!(
                        "identical playOrder values ({order}) for navPoint/navTarget/pageTarget that do not refer to same target"
                    ),
                ));
            }
        }
    }
}

/// **RSC-005** (schema channel) — `epub-nav-30.sch`'s `nav-ocurrence` pattern:
/// the navigation document's `<body>` must contain exactly one `toc` nav, and at
/// most one each of `page-list` and `landmarks`. The nav type is read from
/// `epub:type` **resolved through the namespace**, so a document that binds the
/// `epub` prefix to anything but the EPUB 3 structural-semantics namespace has no
/// typed navs at all — the real-world defect this catches most often.
fn check_nav_occurrence(
    pkg: &opf::Package,
    opf_dir: &str,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    encrypted: &HashSet<String>,
    report: &mut Report,
) {
    let Some(nav) = pkg
        .manifest
        .iter()
        .find(|m| m.properties.iter().any(|p| p == "nav"))
    else {
        return;
    };
    let path = join_opf(opf_dir, &nav.href);
    // An encrypted/obfuscated resource is never content-checked (epubcheck
    // reports RSC-004 and skips it).
    if encrypted.contains(&path) {
        return;
    }
    let Ok(text) = read_text(zip, &path) else {
        return;
    };

    let mut reader = Reader::from_str(&text);
    let mut scopes: Vec<Vec<(Vec<u8>, Vec<u8>)>> = Vec::new();
    let mut has_body = false;
    let mut body_depth: Option<usize> = None;
    let mut counts = HashMap::new();
    let count_nav = |e: &quick_xml::events::BytesStart,
                     scopes: &[Vec<(Vec<u8>, Vec<u8>)>],
                     counts: &mut HashMap<String, usize>| {
        if elem_namespace(e.name().as_ref(), scopes).as_deref() != Some(XHTML_NS)
            || local_name(e.name().as_ref()) != b"nav"
        {
            return;
        }
        let Some(types) = e.attributes().flatten().find(|a| {
            local_name(a.key.as_ref()) == b"type"
                && attr_namespace(a.key.as_ref(), scopes).as_deref() == Some(OPS_NS)
        }) else {
            return;
        };
        for token in String::from_utf8_lossy(&types.value).split_whitespace() {
            *counts.entry(token.to_string()).or_insert(0) += 1;
        }
    };
    loop {
        match reader.read_event() {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                scopes.push(xmlns_frame(&e));
                if elem_namespace(e.name().as_ref(), &scopes).as_deref() == Some(XHTML_NS)
                    && local_name(e.name().as_ref()) == b"body"
                {
                    has_body = true;
                    body_depth.get_or_insert(scopes.len());
                }
                if body_depth.is_some() {
                    count_nav(&e, &scopes, &mut counts);
                }
            }
            Ok(Event::Empty(e)) => {
                scopes.push(xmlns_frame(&e));
                if body_depth.is_some() {
                    count_nav(&e, &scopes, &mut counts);
                }
                scopes.pop();
            }
            Ok(Event::End(_)) => {
                if body_depth == Some(scopes.len()) {
                    body_depth = None;
                }
                scopes.pop();
            }
            _ => {}
        }
    }
    if !has_body {
        return; // the schematron rule is scoped to <body>; no body, no rule
    }
    if counts.get("toc").copied().unwrap_or(0) != 1 {
        report.push(Violation::new(
            Rule::NavTocOccurrence,
            path.clone(),
            "exactly one \"toc\" nav element must be present in the navigation document",
        ));
    }
    for kind in ["page-list", "landmarks"] {
        if counts.get(kind).copied().unwrap_or(0) > 1 {
            report.push(Violation::new(
                Rule::NavTocOccurrence,
                path.clone(),
                format!("multiple occurrences of the {kind:?} nav element"),
            ));
        }
    }
}

/// The `content` of the NCX's `<meta name="dtb:uid">`, if present.
fn ncx_dtb_uid(text: &str) -> Option<String> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e))
                if local_name(e.name().as_ref()) == b"meta" =>
            {
                let (mut name, mut content) = (None, None);
                for a in e.attributes().flatten() {
                    match a.key.as_ref() {
                        b"name" => name = Some(String::from_utf8_lossy(&a.value).to_string()),
                        b"content" => content = Some(String::from_utf8_lossy(&a.value).to_string()),
                        _ => {}
                    }
                }
                if name.as_deref() == Some("dtb:uid") {
                    return content;
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
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
    if extra_len != 0 {
        report.push(Violation::new(
            Rule::MimetypeExtraField,
            "mimetype",
            format!("mimetype entry has a {extra_len}-byte extra field; none is permitted"),
        ));
    }
    // Content must equal `application/epub+zip`. epubcheck reads the mimetype
    // entry *decompressed*, through the zip, and compares the whole string — it
    // does NOT flag the compression method (a Deflated mimetype with the right
    // content is not an epubcheck error, and real books ship them). So the
    // content compare applies only to a STORED entry, whose bytes ARE the content
    // verbatim; a compressed mimetype's raw bytes can't be compared here, so it's
    // left alone rather than misread (which would be a false PKG-007).
    if compression == 0 {
        // A data descriptor (general-purpose bit 3) zeroes the local-header size
        // fields — the real size lives in the trailing descriptor / central
        // directory epubcheck reads. When it's set, `uncomp_size` is 0/unreliable,
        // so fall back to a leading-bytes compare: it can't see trailing garbage,
        // but never misfires on a correct mimetype stored with a data descriptor.
        let flags = u16::from_le_bytes([bytes[6], bytes[7]]);
        let has_data_descriptor = flags & 0x08 != 0;
        let content_start = 30 + name_len + extra_len;
        let uncomp_size = u32::from_le_bytes([bytes[22], bytes[23], bytes[24], bytes[25]]) as usize;
        let content: &[u8] = if !has_data_descriptor && content_start + uncomp_size <= bytes.len() {
            &bytes[content_start..content_start + uncomp_size]
        } else {
            &bytes[content_start..content_start + REQUIRED.len()]
        };
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
                            // The rootfile path is an OCF URL: percent-decode and
                            // NFC-normalize so it matches the zip entry names.
                            let raw = String::from_utf8_lossy(&attr.value);
                            full_path = Some(nfc(&percent_decode(&raw)))
                        }
                        b"media-type" => {
                            media_type = String::from_utf8_lossy(&attr.value).to_string()
                        }
                        _ => {}
                    }
                }
                match full_path {
                    // OPF-016: <rootfile> is missing its required full-path.
                    None => report.push(Violation::new(
                        Rule::RootfileMissingFullPath,
                        "META-INF/container.xml",
                        "<rootfile> element is missing its required full-path attribute",
                    )),
                    // OPF-017: full-path must not be empty.
                    Some(fp) if fp.is_empty() => report.push(Violation::new(
                        Rule::RootfileEmptyFullPath,
                        "META-INF/container.xml",
                        "<rootfile> full-path attribute must not be empty",
                    )),
                    Some(fp) => rootfiles.push((fp, media_type)),
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
        // Remote (http[s]:, …) and data: resources are not expected in the
        // container, so their absence is never RSC-001 (epubcheck checks remote
        // items via the remote-resource rules instead). file: URLs (RSC-030) and
        // hrefs that leak outside the container (RSC-026, path-absolute or too
        // many `..`) are not container paths either — reported by
        // check_parent_paths_in_opf, not as a missing file here.
        if is_remote_href(&item.href)
            || is_data_url(&item.href)
            || url_scheme(&item.href).as_deref() == Some("file")
            || item.href.starts_with('/')
            || opf_href_leaks(opf_dir, &item.href)
        {
            continue;
        }
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

/// NAV-010: within the EPUB 3 navigation document, a hyperlink inside a `toc`,
/// `page-list`, or `landmarks` nav must not point at a remote resource — those
/// navs must reference in-container content documents. (Links to out-of-spine
/// container items are RSC-011; per epubcheck's `NavHandler`, NAV-010 covers only
/// the remote case.) The nav type is the nearest enclosing `<nav epub:type>`.
fn check_nav_remote_links(
    pkg: &opf::Package,
    opf_dir: &str,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    encrypted: &HashSet<String>,
    report: &mut Report,
) {
    let Some(nav) = pkg
        .manifest
        .iter()
        .find(|m| m.properties.iter().any(|p| p == "nav"))
    else {
        return;
    };
    let path = join_opf(opf_dir, &nav.href);
    // An encrypted/obfuscated resource is never content-checked (epubcheck
    // reports RSC-004 and skips it).
    if encrypted.contains(&path) {
        return;
    }
    let Ok(text) = read_text(zip, &path) else {
        return;
    };
    for (kind, href) in nav_remote_links(&text) {
        report.push(Violation::new(
            Rule::NavRemoteLink,
            path.clone(),
            format!("{kind} nav links to remote resource {href:?}"),
        ));
    }
}

/// The `(nav-type, href)` of every remote hyperlink inside a `toc`, `page-list`,
/// or `landmarks` nav in the navigation document `text`. The governing nav type
/// is the nearest enclosing `<nav epub:type>`.
fn nav_remote_links(text: &str) -> Vec<(String, String)> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(false);
    let mut nav_stack: Vec<String> = Vec::new();
    let mut out = Vec::new();
    let mut check_anchor = |e: &quick_xml::events::BytesStart, stack: &[String]| {
        let Some(kind) = stack.last().and_then(|t| {
            t.split_whitespace()
                .find(|t| matches!(*t, "toc" | "page-list" | "landmarks"))
        }) else {
            return;
        };
        for attr in e.attributes().flatten() {
            if local_name(attr.key.as_ref()) == b"href" {
                let href = String::from_utf8_lossy(&attr.value);
                if is_remote_href(&href) {
                    out.push((kind.to_string(), href.into_owned()));
                }
            }
        }
    };
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match local_name(e.name().as_ref()) {
                b"nav" => nav_stack.push(epub_type_attr(&e)),
                b"a" => check_anchor(&e, &nav_stack),
                _ => {}
            },
            Ok(Event::Empty(e)) if local_name(e.name().as_ref()) == b"a" => {
                check_anchor(&e, &nav_stack);
            }
            Ok(Event::End(e)) if local_name(e.name().as_ref()) == b"nav" => {
                nav_stack.pop();
            }
            Ok(Event::Eof) => break,
            Err(_) => break, // nav well-formedness is out of scope here
            _ => {}
        }
    }
    out
}

/// The `epub:type` attribute value of an element (raw, trimmed), or empty. Matches
/// the prefixed `epub:type` or any attribute whose local name is `type`.
fn epub_type_attr(e: &quick_xml::events::BytesStart) -> String {
    for attr in e.attributes().flatten() {
        let key = attr.key.as_ref();
        if key == b"epub:type" || local_name(key) == b"type" {
            return String::from_utf8_lossy(&attr.value).trim().to_string();
        }
    }
    String::new()
}

/// An element attribute's value, selected by its local name (namespace prefix
/// ignored). `None` if absent.
fn attr_by_local(e: &quick_xml::events::BytesStart, local: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| local_name(a.key.as_ref()) == local)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

/// PKG-026: a resource obfuscated with the IDPF Font Obfuscation algorithm must be
/// a font. Reads `META-INF/encryption.xml`, finds every resource encrypted with
/// the IDPF embedding algorithm (the only algorithm epubcheck marks "obfuscated"),
/// and flags any whose manifest media-type is not a font core media type.
fn check_obfuscated_fonts(
    pkg: &opf::Package,
    opf_dir: &str,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    report: &mut Report,
) {
    let Ok(text) = read_text(zip, "META-INF/encryption.xml") else {
        return; // no encryption.xml → nothing obfuscated
    };
    for uri in idpf_obfuscated_uris(&text) {
        // encryption.xml URIs are container-root-relative, as is join_opf(...).
        if let Some(item) = pkg
            .manifest
            .iter()
            .find(|m| join_opf(opf_dir, &m.href) == uri)
            && !is_blessed_font_type(&item.media_type)
        {
            report.push(Violation::new(
                Rule::ObfuscatedResourceNotFont,
                "META-INF/encryption.xml".to_string(),
                format!(
                    "obfuscated resource {uri:?} has media-type {:?}, not a font core media type",
                    item.media_type
                ),
            ));
        }
    }
}

/// Every container-root-relative URI `encryption.xml` encrypts, whatever the
/// algorithm. epubcheck can decrypt none of them (each of its `EncryptionFilter`
/// implementations answers `canDecrypt() == false`, font obfuscation included),
/// so it reports RSC-004 and skips the resource's content checks entirely —
/// judging ciphertext would be a false positive on every content rule. Reading
/// the *declaration* is still fair game (PKG-026 keys on it).
fn encrypted_uris(text: &str) -> HashSet<String> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(false);
    let mut out = HashSet::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e))
                if local_name(e.name().as_ref()) == b"CipherReference" =>
            {
                if let Some(uri) = attr_by_local(&e, b"URI") {
                    out.insert(uri);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// The container-root-relative URIs obfuscated with the IDPF Font Obfuscation
/// algorithm (`http://www.idpf.org/2008/embedding`) in an `encryption.xml`. The
/// governing algorithm is the `<EncryptionMethod>` of the enclosing
/// `<EncryptedData>`, which precedes the `<CipherReference URI>`.
fn idpf_obfuscated_uris(text: &str) -> Vec<String> {
    const IDPF: &str = "http://www.idpf.org/2008/embedding";
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(false);
    let mut out = Vec::new();
    let mut algorithm = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(e.name().as_ref()) {
                b"EncryptedData" => algorithm.clear(),
                b"EncryptionMethod" => {
                    if let Some(a) = attr_by_local(&e, b"Algorithm") {
                        algorithm = a;
                    }
                }
                b"CipherReference" => {
                    if algorithm == IDPF
                        && let Some(uri) = attr_by_local(&e, b"URI")
                    {
                        out.push(uri);
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break, // encryption.xml well-formedness is out of scope here
            _ => {}
        }
    }
    out
}

/// Raster-image resource conformance (epubcheck's `BitmapChecker`), for the three
/// checked bitmap types (`image/jpeg`, `image/gif`, `image/png`):
///
/// - **OPF-029** — the leading bytes don't match the declared format's signature
///   (`checkHeader`). Conservative: only the unambiguous magic numbers, and only
///   when the file has enough bytes to judge.
/// - **PKG-021** — the bytes cannot be decoded to obtain the image's dimensions
///   (`getImageSizes`, "Corrupted image file"). Independent of OPF-029: a
///   wrong-magic-but-decodable file is OPF-029 + PKG-022 (not PKG-021), while a
///   truly garbage or truncated file is both OPF-029 and PKG-021 — matching
///   epubcheck, which can raise the two together.
fn check_image_headers(
    pkg: &opf::Package,
    opf_dir: &str,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    encrypted: &HashSet<String>,
    report: &mut Report,
) {
    for item in &pkg.manifest {
        if !is_checked_bitmap_type(&item.media_type) {
            continue;
        }
        let path = join_opf(opf_dir, &item.href);
        // A resource listed in `META-INF/encryption.xml` is never content-checked
        // (epubcheck reports RSC-004 and skips it): its stored bytes are
        // encrypted/obfuscated, so parsing them judges ciphertext.
        if encrypted.contains(&path) {
            continue;
        }
        let Ok(bytes) = read_bytes(zip, &path) else {
            continue; // remote/data/missing — handled by RSC-006/007 elsewhere
        };
        if image_header_mismatches(&item.media_type, &bytes) {
            report.push(Violation::new(
                Rule::ResourceMediaTypeMismatch,
                path.clone(),
                format!(
                    "resource {path:?} does not match its declared media-type {:?}",
                    item.media_type
                ),
            ));
        }
        if image_fails_to_decode(&bytes) {
            report.push(Violation::new(
                Rule::CorruptImage,
                path.clone(),
                format!(
                    "resource {path:?} (declared {:?}) could not be decoded; the image file is corrupt",
                    item.media_type
                ),
            ));
        }
    }
}

/// RSC-007 / RSC-026 for the references *inside* CSS resources — `url(...)` and
/// `@import` targets, which epubcheck resolves the same way it resolves document
/// references. Each target is resolved relative to the CSS file it appears in: a
/// target that leaks the container is RSC-026, one that resolves to no zip entry
/// is RSC-007; the URL-integrity siblings (RSC-020 space, RSC-030 file:, RSC-033
/// relative-query) are applied too, matching the content-reference pass. (Inline
/// `<style>` / `style=` CSS is not yet scanned — a recall gap, not an FP.)
fn check_css_references(
    pkg: &opf::Package,
    opf_dir: &str,
    epub3: bool,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    encrypted: &HashSet<String>,
    zip_paths: &HashSet<String>,
    report: &mut Report,
) {
    let manifest_paths: HashSet<String> = pkg
        .manifest
        .iter()
        .map(|m| join_opf(opf_dir, &m.href))
        .collect();
    for item in &pkg.manifest {
        if !item.media_type.trim().eq_ignore_ascii_case("text/css") {
            continue;
        }
        let css_path = join_opf(opf_dir, &item.href);
        // A resource listed in `META-INF/encryption.xml` is never content-checked
        // (epubcheck reports RSC-004 and skips it): its stored bytes are
        // encrypted/obfuscated, so parsing them judges ciphertext.
        if encrypted.contains(&css_path) {
            continue;
        }
        let Ok(text) = read_text(zip, &css_path) else {
            continue;
        };
        // CSS-002: `url()` with nothing inside references nothing.
        for _ in 0..css_url_tokens_with_empties(&text).1 {
            report.push(Violation::new(
                Rule::EmptyCssReference,
                css_path.clone(),
                "empty or NULL reference found in a url() value",
            ));
        }
        for href in extract_css_urls(&text) {
            if let Some(why) = invalid_url_reason(&href) {
                report.push(Violation::new(
                    Rule::InvalidUrl,
                    css_path.clone(),
                    format!("CSS reference {href:?} is not a valid URL ({why})"),
                ));
            }
            if href
                .get(..5)
                .is_some_and(|p| p.eq_ignore_ascii_case("file:"))
            {
                report.push(Violation::new(
                    Rule::FileUrlNotAllowed,
                    css_path.clone(),
                    format!("CSS reference {href:?} uses a file: URL, not allowed in EPUB"),
                ));
            }
            if href.split('#').next().unwrap_or(&href).starts_with('/')
                || href_leaks_container(&css_path, &href)
            {
                report.push(Violation::new(
                    Rule::HrefEscapesOpfRoot,
                    css_path.clone(),
                    format!("CSS reference {href:?} leaks outside the container"),
                ));
                // A path-absolute URL resolves against the container root, which
                // this pass cannot express; a `..`-leak clamps at the root (which
                // [`resolve_href`] already does), so it still gets existence-checked.
                if href.split('#').next().unwrap_or(&href).starts_with('/') {
                    continue;
                }
            }
            if let Some(resolved) = resolve_href(&css_path, &href) {
                if url_scheme(&href).is_none()
                    && href.split('#').next().unwrap_or(&href).contains('?')
                {
                    report.push(Violation::new(
                        Rule::RelativeUrlWithQuery,
                        css_path.clone(),
                        format!("relative CSS reference {href:?} must not have a query component"),
                    ));
                    continue;
                }
                // As in the content-reference pass, a *declared* manifest item
                // missing from the container is RSC-001 (reported once against
                // the manifest), never RSC-007.
                if !zip_paths.contains(&resolved) && !manifest_paths.contains(&resolved) {
                    report.push(Violation::new(
                        Rule::BrokenHref,
                        css_path.clone(),
                        format!("CSS reference {href:?} -> {resolved:?} not present in the zip"),
                    ));
                }
            }
        }
        // A `url()`/`@import` target with an opaque scheme (`kindle:embed:…`, …)
        // that no manifest item declares. epubcheck treats any non-`data`,
        // non-same-origin URL as remote; its verdict then depends on the CSS
        // context (a direct port of `ResourceReferencesChecker`):
        //   * inside `@font-face` (a FONT reference, remote-exempt in EPUB 3) it
        //     falls through to the undeclared check → **RSC-008**;
        //   * anywhere else (image/other, or EPUB 2) a remote reference is not
        //     allowed → **RSC-006**.
        // `http`/`https` (declared remote resources are a real EPUB 3 feature —
        // FP-risky), `data:` (inline), and `file:` (RSC-030) are left alone.
        let font_face_targets = css_font_face_url_tokens(&text);
        for tok in css_url_tokens(&text) {
            let Some(scheme) = url_scheme(&tok) else {
                continue;
            };
            if matches!(scheme.as_str(), "http" | "https" | "data" | "file")
                || pkg.manifest.iter().any(|m| m.href == tok)
            {
                continue;
            }
            if epub3 && font_face_targets.contains(&tok) {
                report.push(Violation::new(
                    Rule::ReferenceNotInManifest,
                    css_path.clone(),
                    format!("CSS reference {tok:?} is not declared in the OPF manifest"),
                ));
            } else {
                report.push(Violation::new(
                    Rule::RemoteResourceNotAllowed,
                    css_path.clone(),
                    format!(
                        "CSS reference {tok:?} is a remote resource, not allowed in this context"
                    ),
                ));
            }
        }
    }
}

/// True when `bytes` cannot be decoded to image dimensions — the signal behind
/// PKG-021. A valid raster image always yields its dimensions from the header
/// (`into_dimensions` reads only that, not the full pixel data), so a failure
/// means the format is unrecognized (garbage bytes) or the header is truncated.
/// Mirrors epubcheck raising "Corrupted image file" when Java ImageIO cannot
/// read the image.
fn image_fails_to_decode(bytes: &[u8]) -> bool {
    match image::ImageReader::new(Cursor::new(bytes)).with_guessed_format() {
        Ok(reader) => reader.into_dimensions().is_err(),
        Err(_) => true,
    }
}

fn is_checked_bitmap_type(media_type: &str) -> bool {
    matches!(media_type.trim(), "image/jpeg" | "image/gif" | "image/png")
}

/// True when `bytes` fail the signature for `media_type`. Only the three checked
/// bitmap types are judged, and only with enough bytes; anything else is `false`
/// (not a mismatch).
fn image_header_mismatches(media_type: &str, bytes: &[u8]) -> bool {
    let expect: &[u8] = match media_type.trim() {
        "image/jpeg" => &[0xFF, 0xD8],
        "image/gif" => b"GIF8",
        "image/png" => &[0x89, b'P', b'N', b'G'],
        _ => return false,
    };
    bytes.len() >= expect.len() && !bytes.starts_with(expect)
}

/// epubcheck's `isBlessedFontType`: the font core media types.
fn is_blessed_font_type(media_type: &str) -> bool {
    matches!(
        media_type.trim(),
        "font/otf"
            | "font/ttf"
            | "font/woff"
            | "font/woff2"
            | "application/font-sfnt"
            | "application/font-woff"
            | "application/vnd.ms-opentype"
            | "application/x-font-ttf"
    )
}

fn check_nav_present(pkg: &opf::Package, epub3: bool, opf_path: &str, report: &mut Report) {
    if !epub3 {
        return; // the EPUB 3 nav document is defined only for EPUB 3
    }
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

#[allow(clippy::too_many_arguments)]
fn check_xhtml_hrefs_and_reachability(
    pkg: &opf::Package,
    opf_dir: &str,
    epub2: bool,
    epub3: bool,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    encrypted: &HashSet<String>,
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
    // (source_path, raw_href) for every reference carrying a `#fragment`. The
    // package document's own `<guide>` references are among them: epubcheck
    // resolves them like any other reference and reports RSC-012 against the OPF.
    let mut fragment_refs: Vec<(String, String)> = pkg
        .guide_hrefs
        .iter()
        .filter(|href| href.contains('#'))
        .map(|href| (opf_path.to_string(), href.clone()))
        .collect();
    // Manifest-declared resource paths, for RSC-008 (a reference resolving to a
    // file that is in the container but undeclared). Reported once per target.
    let manifest_paths: HashSet<String> = pkg
        .manifest
        .iter()
        .map(|m| join_opf(opf_dir, &m.href))
        .collect();
    let mut undeclared_reported: HashSet<String> = HashSet::new();
    // For RSC-010/011: resolve a hyperlink target back to its manifest item, its
    // fallback chain (by id), and spine membership.
    let manifest_by_path: HashMap<String, &opf::ManifestItem> = pkg
        .manifest
        .iter()
        .map(|m| (join_opf(opf_dir, &m.href), m))
        .collect();
    let by_id: HashMap<&str, &opf::ManifestItem> =
        pkg.manifest.iter().map(|m| (m.id.as_str(), m)).collect();
    let spine_ids: HashSet<&str> = pkg.spine.iter().map(|s| s.idref.as_str()).collect();
    // OPF-096's script exemption: a non-linear spine item that no hyperlink
    // reaches is only an error when the publication has no scripts (epubcheck
    // downgrades it to OPF-096b USAGE otherwise, because a script may navigate to
    // it). Seed from the declared `scripted` manifest property; also set below if
    // any content document actually contains a `<script>` element.
    let mut has_scripts = pkg
        .manifest
        .iter()
        .any(|m| m.properties.iter().any(|p| p == "scripted"));

    for item in &pkg.manifest {
        if !is_xhtml(&item.media_type) {
            continue;
        }
        let path = join_opf(opf_dir, &item.href);
        // A resource listed in `META-INF/encryption.xml` is never content-checked
        // (epubcheck reports RSC-004 and skips it): its stored bytes are
        // encrypted/obfuscated, so parsing them judges ciphertext.
        if encrypted.contains(&path) {
            continue;
        }
        let Ok(text) = read_text(zip, &path) else {
            continue;
        };
        if text.contains("<script") {
            has_scripts = true;
        }
        doc_ids.insert(path.clone(), collect_element_ids(&text));
        for (kind, href) in collect_references(&text) {
            // RSC-020: the URL is rejected by the WHATWG parser (see
            // [`invalid_url_reason`]). Reported alongside the other checks; it does
            // not stop resolution (epubcheck also continues via lenient parse).
            if let Some(why) = invalid_url_reason(&href) {
                report.push(Violation::new(
                    Rule::InvalidUrl,
                    path.clone(),
                    format!("reference {href:?} is not a valid URL ({why})"),
                ));
            }
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
            // RSC-026: a reference that is not a valid relative OCF URL leaks
            // outside the container — either path-absolute ("/foo") / scheme-
            // relative ("//host/foo"), or rising above the container root via too
            // many `..`. Fully-absolute URLs with a scheme are external and
            // filtered by resolve_href returning None.
            if href.split('#').next().unwrap_or(&href).starts_with('/')
                || href_leaks_container(&path, &href)
            {
                report.push(Violation::new(
                    Rule::HrefEscapesOpfRoot,
                    path.clone(),
                    format!("reference {href:?} leaks outside the container"),
                ));
                // The WHATWG parser clamps surplus `..` at the container root and
                // the reference is then existence-checked like any other (so a
                // leaking href that names a missing file is RSC-026 *and* RSC-007)
                // — [`resolve_href`] clamps the same way. A path-absolute URL
                // instead resolves against the container root, which this pass
                // cannot express, so it stops here.
                if href.split('#').next().unwrap_or(&href).starts_with('/') {
                    continue;
                }
            }
            if let Some(resolved) = resolve_href(&path, &href) {
                // RSC-033: a *relative* URL must not carry a query component. The
                // '?' would otherwise be swallowed into the resolved path and
                // misfire as a broken href, so handle it first and skip the rest.
                // A scheme'd URL (e.g. `kindle:embed:0007?mime=…`) is not a relative
                // reference — its '?' is part of an opaque path — so it's left to
                // the resolution/existence checks below (epubcheck resolves it and
                // reports RSC-007, not RSC-033).
                if url_scheme(&href).is_none()
                    && href.split('#').next().unwrap_or(&href).contains('?')
                {
                    report.push(Violation::new(
                        Rule::RelativeUrlWithQuery,
                        path.clone(),
                        format!("relative reference {href:?} must not have a query component"),
                    ));
                    continue;
                }
                if kind == RefKind::Hyperlink {
                    hyperlink_targets.insert(resolved.clone());
                    // RSC-010/011: a hyperlink to a *declared, present* resource
                    // must point at an EPUB content document (or a type with a
                    // content-document fallback) that is a spine item. Undeclared
                    // or missing targets are RSC-007/008, handled below; epubcheck
                    // runs this only for a resolved manifest item. EPUB CFI
                    // fragments are out of scope (epubcheck skips them too).
                    let frag = href.split_once('#').map(|(_, f)| f).unwrap_or("");
                    if zip_paths.contains(&resolved)
                        && !frag.starts_with("epubcfi(")
                        && let Some(target) = manifest_by_path.get(&resolved)
                        && let Some(rule) = hyperlink_target_rule(target, &by_id, &spine_ids, epub2)
                    {
                        let mt = &target.media_type;
                        let detail = if rule == Rule::HyperlinkToNonContentDocument {
                            format!(
                                "hyperlink {href:?} -> {resolved:?} targets media-type {mt:?}, not an EPUB content document"
                            )
                        } else {
                            format!(
                                "hyperlink {href:?} -> {resolved:?} targets a resource that is not a spine item"
                            )
                        };
                        report.push(Violation::new(rule, path.clone(), detail));
                    }
                }
                if !zip_paths.contains(&resolved) && !manifest_paths.contains(&resolved) {
                    // RSC-007 is the *undeclared*-target message: epubcheck only
                    // reaches it when the target is not a registered publication
                    // resource (`ResourceReferencesChecker.checkUndeclaredReference`).
                    // A declared manifest item missing from the container is
                    // RSC-001, reported once against the manifest instead.
                    report.push(Violation::new(
                        Rule::BrokenHref,
                        path.clone(),
                        format!("reference {href:?} -> {resolved:?} not present in the zip"),
                    ));
                } else if resolved != opf_path
                    && !manifest_paths.contains(&resolved)
                    && undeclared_reported.insert(resolved.clone())
                {
                    // RSC-008: present in the container, but not a declared
                    // publication resource.
                    report.push(Violation::new(
                        Rule::ReferenceNotInManifest,
                        path.clone(),
                        format!(
                            "reference {href:?} -> {resolved:?} is in the container but not declared in the manifest"
                        ),
                    ));
                }
            }
        }
    }

    // The NCX (EPUB 2 nav) is not an XHTML content document, so its
    // `<content src="…#frag">` targets were not scanned in the loop above — but
    // epubcheck resolves those fragments too, firing RSC-012 on the NCX exactly
    // as it does on a nav.xhtml href. Feed them through the same check (source =
    // the NCX path; targets resolve against the already-built `doc_ids`). Each
    // NCX target is ALSO a hyperlink for reachability: epubcheck's NCXHandler
    // registers `<content src>` as `Reference.Type.HYPERLINK`, so it satisfies
    // OPF-096 (non-linear reachable) just like an `<a href>` — a cover reached
    // only from the NCX toc is reachable, not an error.
    if let Some(ncx) = pkg.manifest.iter().find(|m| {
        m.media_type
            .eq_ignore_ascii_case("application/x-dtbncx+xml")
    }) {
        let ncx_path = join_opf(opf_dir, &ncx.href);
        if let Ok(text) = read_text(zip, &ncx_path) {
            let mut ncx_broken_reported: HashSet<String> = HashSet::new();
            for src in collect_ncx_content_srcs(&text) {
                if let Some(target) = resolve_href(&ncx_path, &src) {
                    // RSC-007: an NCX `<content src>` target must exist in the
                    // container, exactly as a nav.xhtml `<a href>` target must
                    // (epubcheck resolves both the same way). Reported once per
                    // missing target.
                    if !zip_paths.contains(&target)
                        && !manifest_paths.contains(&target)
                        && ncx_broken_reported.insert(target.clone())
                    {
                        report.push(Violation::new(
                            Rule::BrokenHref,
                            ncx_path.clone(),
                            format!(
                                "NCX navigation target {src:?} -> {target:?} not present in the zip"
                            ),
                        ));
                    }
                    // RSC-010/011: an NCX `<content src>` is a HYPERLINK reference
                    // (epubcheck's NCXHandler), so a present, manifest-declared
                    // target must be a content document that is a spine item — the
                    // same verdict [`hyperlink_target_rule`] applies to an `<a href>`.
                    let frag = src.split_once('#').map(|(_, f)| f).unwrap_or("");
                    if zip_paths.contains(&target)
                        && !frag.starts_with("epubcfi(")
                        && let Some(item) = manifest_by_path.get(&target)
                        && let Some(rule) = hyperlink_target_rule(item, &by_id, &spine_ids, epub2)
                    {
                        let detail = if rule == Rule::HyperlinkToNonContentDocument {
                            format!(
                                "NCX navigation target {src:?} -> {target:?} targets media-type {:?}, not an EPUB content document",
                                item.media_type
                            )
                        } else {
                            format!(
                                "NCX navigation target {src:?} -> {target:?} targets a resource that is not a spine item"
                            )
                        };
                        report.push(Violation::new(rule, ncx_path.clone(), detail));
                    }
                    hyperlink_targets.insert(target);
                }
                if src.contains('#') {
                    fragment_refs.push((ncx_path.clone(), src));
                }
            }
        }
    }

    check_fragments(&doc_ids, &fragment_refs, report);

    // Reachability: every spine item with `linear="no"` must be the target of
    // some hyperlink elsewhere in the publication — unless the publication has
    // scripts, which may navigate to it (epubcheck's OPF-096b USAGE case).
    // OPF-096 is an EPUB 3 rule (epubcheck emits it only from `OPFChecker30`);
    // EPUB 2 has no non-linear-reachability requirement, so it never fires there.
    for s in pkg.spine.iter().filter(|_| epub3 && !has_scripts) {
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
        // Remote/data hrefs don't resolve to a container path.
        if is_remote_href(&item.href) || is_data_url(&item.href) {
            continue;
        }
        // RSC-030: a file: URL is never allowed in an EPUB.
        if url_scheme(&item.href).as_deref() == Some("file") {
            report.push(Violation::new(
                Rule::FileUrlNotAllowed,
                opf_path,
                format!(
                    "manifest item href={:?} uses a file: URL, not allowed in EPUB",
                    item.href
                ),
            ));
            continue;
        }
        // RSC-026: a path-absolute manifest href ("/EPUB/x") or one that rises
        // above the container root (too many `..`) is not a valid relative OCF
        // URL — it leaks outside the container. (join_opf still clamps it to a
        // container path so it isn't double-reported as missing/undeclared.)
        if item.href.starts_with('/') || opf_href_leaks(opf_dir, &item.href) {
            report.push(Violation::new(
                Rule::HrefEscapesOpfRoot,
                opf_path,
                format!(
                    "manifest item href={:?} leaks outside the container",
                    item.href
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

/// Fine-grained reference type for the remote-resource rule (RSC-006), a reduced
/// form of epubcheck's `Reference.Type` keeping only the distinctions RSC-006
/// keys on: `LinkLike` (`<a>`/`<area>`/non-stylesheet `<link>` — always allowed
/// remote), `Audio`/`Video` (allowed remote in EPUB 3 by element type), and
/// `Other` (`<img>`, stylesheet `<link>`, `<object>`, `<iframe>`, `<script>`, … —
/// not allowed remote unless the *declared* target is itself an audio/video/font
/// media type). A remote font is exempted through that target-media-type path (a
/// font reference is only ever declared, via `@font-face` or the manifest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefType {
    LinkLike,
    Audio,
    Video,
    Other,
}

/// Every reference in `content` paired with its [`RefType`], tracking the nearest
/// `<audio>`/`<video>`/`<picture>` ancestor so a `<source>` is typed by its
/// parent (audio source → `Audio`, video source → `Video`, picture source →
/// `Other`/image). Used by the remote-resource check; the fragment/existence
/// checks use the coarser [`collect_references`].
fn typed_content_refs(content: &str) -> Vec<(RefType, String)> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(false);
    let mut out = Vec::new();
    // Media context of each open ancestor: "audio" / "video" / "picture" / "".
    let mut stack: Vec<&'static str> = Vec::new();
    let mut base = BaseUrl::default();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                base.visit(&e);
                classify_element_refs(&e, stack.last().copied().unwrap_or(""), &base, &mut out);
                let name = e.name();
                stack.push(match local_name(name.as_ref()) {
                    b"audio" => "audio",
                    b"video" => "video",
                    b"picture" => "picture",
                    _ => "",
                });
            }
            Ok(Event::Empty(e)) => {
                base.visit(&e);
                classify_element_refs(&e, stack.last().copied().unwrap_or(""), &base, &mut out);
            }
            Ok(Event::End(_)) => {
                stack.pop();
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Emit `(RefType, href)` for each reference-bearing attribute of one element,
/// given the media context (`parent`) for a `<source>` and the base URL in scope.
fn classify_element_refs(
    e: &quick_xml::events::BytesStart<'_>,
    parent: &str,
    base: &BaseUrl,
    out: &mut Vec<(RefType, String)>,
) {
    let name = e.name();
    let local = local_name(name.as_ref());
    let mut rel = String::new();
    if local == b"link" {
        for attr in e.attributes().flatten() {
            if local_name(attr.key.as_ref()) == b"rel" {
                rel = String::from_utf8_lossy(&attr.value).to_ascii_lowercase();
            }
        }
    }
    for attr in e.attributes().flatten() {
        let akey = local_name(attr.key.as_ref());
        let ty = match (local, akey) {
            (b"a" | b"area", b"href") => RefType::LinkLike,
            (b"link", b"href") => {
                if rel.split_whitespace().any(|k| k == "stylesheet") {
                    RefType::Other
                } else {
                    RefType::LinkLike
                }
            }
            (b"audio", b"src") => RefType::Audio,
            (b"video", b"src") => RefType::Video,
            (b"video", b"poster") => RefType::Other,
            (b"track", b"src") => RefType::Other,
            (b"source", b"src") => match parent {
                "audio" => RefType::Audio,
                "video" => RefType::Video,
                _ => RefType::Other,
            },
            (b"object", b"data") => RefType::Other,
            (b"img" | b"image" | b"use", b"href" | b"src") => RefType::Other,
            (b"embed" | b"iframe" | b"script", b"src") => RefType::Other,
            _ => continue,
        };
        let val = String::from_utf8_lossy(&attr.value).to_string();
        if !val.is_empty() {
            out.push((ty, base.resolve(&val)));
        }
    }
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
    let mut base = BaseUrl::default();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                base.visit(&e);
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
                            out.push((kind, base.resolve(&val)));
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

/// Remove CSS `/* … */` comments so a commented-out `url()`/`@import` never
/// counts as a reference (an unterminated comment swallows the rest, as a CSS
/// parser would). Comments do not nest in CSS.
fn strip_css_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => return out, // unterminated comment → rest is all comment
        }
    }
    out.push_str(rest);
    out
}

/// Case-insensitive byte search for the ASCII keyword `kw` in `hay`, returning
/// its byte offset. `kw` is ASCII, so the returned offset is a char boundary.
fn find_ascii_ci(hay: &str, kw: &[u8]) -> Option<usize> {
    let hb = hay.as_bytes();
    if kw.is_empty() || hb.len() < kw.len() {
        return None;
    }
    (0..=hb.len() - kw.len()).find(|&i| hb[i..i + kw.len()].eq_ignore_ascii_case(kw))
}

/// The container-relative resource targets referenced from a CSS resource, via
/// `url(...)` functions and `@import` rules (comments stripped first). Quotes
/// are removed and the `url`/`@import` keywords matched case-insensitively (CSS
/// is ASCII-case-insensitive there). Absolute-URL, `data:`, and pure-`#fragment`
/// targets are dropped — only references that resolve to a container path feed
/// the existence/leak checks (a remote CSS resource is a separate concern).
fn extract_css_urls(css_raw: &str) -> Vec<String> {
    css_url_tokens(css_raw)
        .iter()
        .filter_map(|t| css_url_target(t))
        .collect()
}

/// Every raw `url(...)` / `@import` target in a CSS resource (comments stripped,
/// one layer of quotes removed, empties dropped) — *unclassified*. Callers filter
/// by kind: [`css_url_target`] keeps the container-relative ones (existence/leak
/// checks), while the RSC-008 pass keeps the opaque-scheme ones.
fn css_url_tokens(css_raw: &str) -> Vec<String> {
    css_url_tokens_with_empties(css_raw).0
}

/// The `url()`/`@import` targets of a stylesheet plus the number of *empty*
/// ones — `url()` with nothing inside is CSS-002 ("empty or NULL reference"),
/// which the reference passes cannot see once the token is dropped.
fn css_url_tokens_with_empties(css_raw: &str) -> (Vec<String>, usize) {
    let css = strip_css_comments(css_raw);
    let mut out = Vec::new();
    let mut empties = 0;
    // url( … ) — also covers `@import url(…)`.
    let mut from = 0;
    while let Some(rel) = find_ascii_ci(&css[from..], b"url(") {
        let open = from + rel + 4;
        let Some(close_rel) = css[open..].find(')') else {
            break;
        };
        let tok = unquote(css[open..open + close_rel].trim()).trim();
        if tok.is_empty() {
            empties += 1;
        } else {
            out.push(tok.to_string());
        }
        from = open + close_rel + 1;
    }
    // @import "…" (the string form; the `@import url(…)` form is caught above).
    let mut from = 0;
    while let Some(rel) = find_ascii_ci(&css[from..], b"@import") {
        let after = &css[from + rel + 7..];
        let seg = after.trim_start();
        if let Some(quote @ ('"' | '\'')) = seg.chars().next()
            && let Some(end) = seg[1..].find(quote)
        {
            let tok = seg[1..1 + end].trim();
            if !tok.is_empty() {
                out.push(tok.to_string());
            }
        }
        from = from + rel + 7;
    }
    (out, empties)
}

/// The `url(...)` targets that appear inside `@font-face` rules — the CSS
/// contexts epubcheck types as FONT references (remote-exempt in EPUB 3, so an
/// undeclared remote font there is RSC-008 rather than RSC-006). `@font-face`
/// rules do not nest, so each is the run from its `{` to the next `}`.
fn css_font_face_url_tokens(css_raw: &str) -> HashSet<String> {
    let css = strip_css_comments(css_raw);
    let mut out = HashSet::new();
    let mut from = 0;
    while let Some(rel) = find_ascii_ci(&css[from..], b"@font-face") {
        let after = from + rel + "@font-face".len();
        let Some(open_rel) = css[after..].find('{') else {
            break;
        };
        let open = after + open_rel + 1;
        let close = css[open..].find('}').map_or(css.len(), |c| open + c);
        for tok in css_url_tokens(&css[open..close]) {
            out.insert(tok);
        }
        from = close;
    }
    out
}

/// Strip one layer of matching `"`/`'` quotes from a CSS token, if present.
fn unquote(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Normalize a raw CSS `url()`/`@import` value into a container-relative target,
/// or `None` if it is not one (empty, a pure `#fragment`, a `data:` URL, or an
/// absolute URL with a scheme).
fn css_url_target(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('#') || is_data_url(raw) || url_scheme(raw).is_some() {
        return None;
    }
    Some(raw.to_string())
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

/// Content-document features epubcheck's `OPSHandler30` derives to decide the
/// *required* manifest properties (OPF-014) and the fixed-layout viewport
/// requirement (HTM-046). Every signal here is one epubcheck also raises, and
/// detection is deliberately a **subset** of epubcheck's: under-detecting only
/// misses a finding (a recall gap), while over-detecting would fire OPF-014
/// where epubcheck stays silent (a false positive) — so the port errs toward
/// silence (e.g. it does not scan CSS `url()` or inline event-handler attributes
/// for scripting/remote resources).
#[derive(Default)]
struct ContentFeatures {
    /// An inline `<svg>` element → the `svg` property is required.
    inline_svg: bool,
    /// A MathML `<math>` element → the `mathml` property is required.
    mathml: bool,
    /// A `<script>` with a JavaScript type (or none) or a `<form>` element → the
    /// `scripted` property is required.
    scripted: bool,
    /// A resource load from a remote (absolute-URL) origin → the
    /// `remote-resources` property is required.
    remote_resources: bool,
    /// A `<meta name="viewport">` element is present (satisfies HTM-046).
    has_viewport: bool,
    /// The `content` of the *first* `<meta name="viewport">`, which is the only
    /// one epubcheck parses (later ones are the USAGE-level HTM-060 cases).
    viewport_content: Option<String>,
    /// A `<link rel>` naming both `alternate` and `stylesheet` with no non-empty
    /// `title` — CSS-015.
    untitled_alternate_stylesheet: bool,
}

fn detect_content_features(content: &str) -> ContentFeatures {
    let mut f = ContentFeatures::default();
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name();
                let local = local_name(name.as_ref());
                match local {
                    b"svg" => f.inline_svg = true,
                    b"math" => f.mathml = true,
                    b"form" => f.scripted = true,
                    b"script" if script_element_is_javascript(&e) => f.scripted = true,
                    b"meta" if attr_local_eq(&e, b"name", "viewport") => {
                        if !f.has_viewport {
                            f.viewport_content =
                                Some(attr_by_local(&e, b"content").unwrap_or_default());
                        }
                        f.has_viewport = true;
                    }
                    b"link" => {
                        let rel = attr_by_local(&e, b"rel")
                            .unwrap_or_default()
                            .to_ascii_lowercase();
                        let types: Vec<&str> = rel.split_whitespace().collect();
                        if types.contains(&"alternate")
                            && types.contains(&"stylesheet")
                            && attr_by_local(&e, b"title").unwrap_or_default().is_empty()
                        {
                            f.untitled_alternate_stylesheet = true;
                        }
                    }
                    _ => {}
                }
                if !f.remote_resources && element_loads_remote_resource(local, &e) {
                    f.remote_resources = true;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break, // XHTML well-formedness is out of scope here.
            _ => {}
        }
    }
    f
}

/// The `<meta name="viewport">` microsyntax, a faithful port of epubcheck's
/// `org.w3c.epubcheck.util.microsyntax.ViewportMeta`: a `name=value` list
/// separated by `,`/`;`/whitespace. Returns `Err` on the first parse error (the
/// parser bails there, exactly as the Java one does) and otherwise the
/// properties in document order, duplicates kept — HTM-059 needs to count them.
///
/// The state machine is transcribed rather than re-derived, including the
/// deliberate Java fall-through from the space-or-separator state into the
/// separator state.
fn parse_viewport_meta(s: &str) -> Result<Vec<(String, String)>, ()> {
    #[derive(PartialEq)]
    enum State {
        Name,
        Assign,
        Value,
        Separator,
        SpaceOrSeparator,
    }
    let mut out: Vec<(String, String)> = Vec::new();
    if s.is_empty() {
        return Err(());
    }
    let chars: Vec<char> = s.chars().collect();
    let (mut name, mut value) = (String::new(), String::new());
    let mut state = State::Name;
    let mut i = 0;
    let mut consume = true;
    let mut c = ' ';
    let ws = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0C');
    while !consume || i < chars.len() {
        if consume {
            c = chars[i];
            i += 1;
        } else {
            consume = true;
        }
        match state {
            State::Name => {
                if ws(c) && name.is_empty() {
                    // skip leading whitespace
                } else if c == '=' || ws(c) {
                    state = State::Assign;
                    consume = false;
                } else if c == ',' || c == ';' {
                    state = State::Separator;
                    consume = false;
                } else {
                    name.push(c);
                }
            }
            State::Assign => {
                if name.is_empty() {
                    return Err(()); // NAME_EMPTY
                } else if ws(c) {
                    // skip whitespace
                } else if c == '=' {
                    state = State::Value;
                } else if c == ',' || c == ';' {
                    state = State::Separator;
                    consume = false;
                } else {
                    return Err(()); // VALUE_EMPTY — no '=' matched
                }
            }
            State::Value => {
                if ws(c) && value.is_empty() {
                    // skip whitespace, the value hasn't started
                } else if c == ',' || c == ';' || ws(c) {
                    if value.is_empty() {
                        return Err(()); // VALUE_EMPTY
                    }
                    state = State::SpaceOrSeparator;
                    consume = false;
                } else if c == '=' {
                    return Err(()); // ASSIGN_UNEXPECTED
                } else {
                    value.push(c);
                }
            }
            // Java falls through from SPACE_OR_SEPARATOR into SEPARATOR.
            State::SpaceOrSeparator | State::Separator => {
                if state == State::SpaceOrSeparator {
                    if ws(c) {
                        continue; // skip whitespace
                    }
                    state = State::Separator;
                    consume = false;
                }
                if name.is_empty() {
                    return Err(()); // LEADING_SEPARATOR
                }
                if c == ',' || c == ';' || ws(c) {
                    // skip repeating separators
                } else {
                    out.push((std::mem::take(&mut name), std::mem::take(&mut value)));
                    state = State::Name;
                    consume = false;
                }
            }
        }
    }
    if state == State::Value && value.is_empty() {
        return Err(()); // VALUE_EMPTY
    }
    out.push((name, value));
    if state == State::Separator {
        return Err(()); // TRAILING_SEPARATOR
    }
    Ok(out)
}

/// `ViewportMeta.isValidProperty` — `width`/`height` take a number or the
/// matching `device-*` keyword; any other property name is unconstrained.
fn viewport_value_is_valid(property: &str, value: &str) -> bool {
    let numeric = || {
        let (int, frac) = match value.split_once('.') {
            Some((i, f)) => (i, Some(f)),
            None => (value, None),
        };
        !int.is_empty()
            && int.bytes().all(|b| b.is_ascii_digit())
            && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
    };
    match property {
        "width" => value == "device-width" || numeric(),
        "height" => value == "device-height" || numeric(),
        _ => true,
    }
}

/// The fixed-layout viewport rules over one content document's first viewport
/// meta (epubcheck `OPSHandler30::processMeta`): a syntax error is **HTM-047**;
/// otherwise `width` and `height` must each be present (**HTM-056**), declared
/// once (**HTM-059**) and valid (**HTM-057**).
fn check_viewport_meta(content: &str, path: &str, report: &mut Report) {
    let Ok(props) = parse_viewport_meta(content) else {
        report.push(Violation::new(
            Rule::ViewportSyntax,
            path.to_string(),
            format!("viewport metadata {content:?} has a syntax error"),
        ));
        return;
    };
    for property in ["width", "height"] {
        let values: Vec<&str> = props
            .iter()
            .filter(|(n, _)| n == property)
            .map(|(_, v)| v.as_str())
            .collect();
        let Some(first) = values.first() else {
            report.push(Violation::new(
                Rule::ViewportDimensionMissing,
                path.to_string(),
                format!(
                    "viewport metadata has no {property:?} dimension (both \"width\" and \"height\" are required)"
                ),
            ));
            continue;
        };
        if values.len() > 1 {
            report.push(Violation::new(
                Rule::ViewportPropertyRepeated,
                path.to_string(),
                format!("viewport {property:?} property is defined more than once: {values:?}"),
            ));
        }
        if !viewport_value_is_valid(property, first) {
            report.push(Violation::new(
                Rule::ViewportValueInvalid,
                path.to_string(),
                format!(
                    "viewport {property:?} value must be a positive number or the keyword \"device-{property}\", not {first:?}"
                ),
            ));
        }
    }
}

/// True if a `<script>` element's `type` makes it executable JavaScript — a
/// faithful port of epubcheck's `OPFChecker.isScriptType` (a missing `type`
/// defaults to JavaScript). A non-JS type (e.g. `application/ld+json`) is a data
/// block, not scripting, so it does not require the `scripted` property.
fn script_element_is_javascript(e: &quick_xml::events::BytesStart<'_>) -> bool {
    let Some(ty) = e
        .attributes()
        .flatten()
        .find(|a| local_name(a.key.as_ref()) == b"type")
    else {
        return true; // no type → JavaScript
    };
    matches!(
        String::from_utf8_lossy(&ty.value)
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "application/javascript"
            | "text/javascript"
            | "application/ecmascript"
            | "application/x-ecmascript"
            | "application/x-javascript"
            | "text/ecmascript"
            | "text/javascript1.0"
            | "text/javascript1.1"
            | "text/javascript1.2"
            | "text/javascript1.3"
            | "text/javascript1.4"
            | "text/javascript1.5"
            | "text/jscript"
            | "text/livescript"
            | "text/x-ecmascript"
            | "text/x-javascript"
    )
}

/// True if any attribute of `e` with local name `key` has the trimmed value `val`.
fn attr_local_eq(e: &quick_xml::events::BytesStart<'_>, key: &[u8], val: &str) -> bool {
    e.attributes().flatten().any(|a| {
        local_name(a.key.as_ref()) == key && String::from_utf8_lossy(&a.value).trim() == val
    })
}

/// True when `e` loads a resource from a remote origin through one of the
/// resource-URL attributes epubcheck routes to its remote-resources check.
/// Hyperlinks (`<a>`/`<area>`) and `<link>` are excluded: a hyperlink is not a
/// resource load, and `<link>` remote handling differs — excluding them keeps
/// OPF-014 free of false positives at the cost of missing the rare remote
/// stylesheet (a recall gap).
fn element_loads_remote_resource(local: &[u8], e: &quick_xml::events::BytesStart<'_>) -> bool {
    let attrs: &[&[u8]] = match local {
        b"img" | b"image" | b"use" | b"embed" | b"iframe" | b"script" | b"audio" | b"source"
        | b"track" => &[b"src", b"href"],
        b"object" => &[b"data"],
        b"video" => &[b"src", b"poster"],
        _ => return false,
    };
    e.attributes().flatten().any(|a| {
        attrs.contains(&local_name(a.key.as_ref()))
            && is_remote_href(String::from_utf8_lossy(&a.value).trim())
    })
}

/// Every `<content src>` value in an NCX — the navigation targets epubcheck
/// resolves for RSC-012, the same way it resolves nav.xhtml hrefs. `local_name`
/// tolerates a namespace prefix on either the element or the attribute.
fn collect_ncx_content_srcs(content: &str) -> Vec<String> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(false);
    let mut out = Vec::new();
    let mut base = BaseUrl::default();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                base.visit(&e);
                if local_name(e.name().as_ref()) != b"content" {
                    continue;
                }
                for attr in e.attributes().flatten() {
                    if local_name(attr.key.as_ref()) == b"src" {
                        let val = String::from_utf8_lossy(&attr.value).to_string();
                        if !val.is_empty() {
                            out.push(base.resolve(&val));
                        }
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
        // decoding, so we leave it rather than risk a false positive. A media
        // fragment (`#xywh=…`, `#t=…` — used by region-based navigation) contains
        // `=`, which no XML id does, so it is not an id reference either.
        if frag.is_empty()
            || frag.starts_with("epubcfi(")
            || frag.contains('%')
            || frag.contains('=')
        {
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
    resolve_container(opf_dir, href).0
}

/// True when resolving a manifest/OPF `href` against `opf_dir` would rise above
/// the container root (a `..` with nothing left to pop) — RSC-026: the href is
/// not a valid relative OCF URL and leaks outside the container.
fn opf_href_leaks(opf_dir: &str, href: &str) -> bool {
    resolve_container(opf_dir, href).1
}

/// Resolve an OCF `href` (a URL) against the OPF directory into a zip-relative
/// container path, returning `(path, leaked)`. `.`/`..` segments are collapsed
/// (RFC 3986 style) and each remaining segment is percent-decoded to match the
/// literal zip entry names. A `..` that would rise above the container root is
/// clamped (the path stays in-container) and reported via `leaked`. A
/// path-absolute href (`/x`) resolves against the container root.
fn resolve_container(opf_dir: &str, href: &str) -> (String, bool) {
    let (mut parts, rest): (Vec<String>, &str) = if let Some(rooted) = href.strip_prefix('/') {
        (Vec::new(), rooted)
    } else if opf_dir.is_empty() {
        (Vec::new(), href)
    } else {
        (opf_dir.split('/').map(String::from).collect(), href)
    };
    let mut leaked = false;
    for seg in rest.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                if parts.pop().is_none() {
                    leaked = true;
                }
            }
            other => parts.push(percent_decode(other)),
        }
    }
    (nfc(&parts.join("/")), leaked)
}

/// Percent-decode a URL path (`%20` → space, UTF-8 aware). A manifest/reference
/// `href` is a URL, but zip entry names are literal — so decode the href before
/// matching (a file named `a b.xhtml` referenced as `a%20b.xhtml` must resolve).
/// Invalid/truncated escapes are left literal.
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex_digit(b[i + 1]), hex_digit(b[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Normalize an OCF path to Unicode NFC. A filename may be stored decomposed (NFD,
/// e.g. macOS-authored content: `u`+combining-diaeresis) yet referenced composed
/// (NFC: precomposed `ü`) or vice versa. epubcheck compares OCF paths in NFC, so
/// bokai normalizes every container path to NFC before matching.
fn nfc(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    if s.is_ascii() {
        return s.to_string();
    }
    s.nfc().collect()
}

/// The in-scope base URL of a document, tracked exactly as epubcheck's
/// `BaseURLHandler` tracks it: a single value, updated in document order by an
/// HTML `<base href>` element or by any element's `xml:base` attribute, and
/// *never restored* when that element ends. Every reference is resolved against
/// it before any other check runs, so once the base is remote every relative
/// reference in the rest of the document is a remote reference (RSC-006's
/// jurisdiction) rather than a container path.
///
/// `None` — the common case — means the base is still the document's own URL,
/// and [`BaseUrl::resolve`] hands hrefs back untouched.
#[derive(Default)]
struct BaseUrl(Option<String>);

impl BaseUrl {
    /// Apply one element's base declaration, if it carries one. An HTML `<base>`
    /// takes its value from `href`; any other element from `xml:base` (epubcheck
    /// reads only one or the other, never both from the same element).
    fn visit(&mut self, e: &quick_xml::events::BytesStart) {
        let name = e.name();
        let key: &[u8] = if local_name(name.as_ref()) == b"base" {
            b"href"
        } else {
            b"xml:base"
        };
        let Some(new) = e
            .attributes()
            .flatten()
            .find(|a| a.key.as_ref() == key)
            .map(|a| String::from_utf8_lossy(&a.value).into_owned())
        else {
            return;
        };
        // A new base is itself resolved against the base already in scope.
        self.0 = Some(match &self.0 {
            Some(base) => resolve_against_base(base, &new),
            None => new,
        });
    }

    /// The effective value of one reference under the base in scope.
    fn resolve(&self, href: &str) -> String {
        match &self.0 {
            Some(base) => resolve_against_base(base, href),
            None => href.to_string(),
        }
    }
}

/// Split a URL into `(origin, path)` — everything up to and including the
/// authority, and the rest. A URL with no authority has an empty origin, so a
/// document-relative base flows through the same merge path as an absolute one.
fn split_url_authority(url: &str) -> (&str, &str) {
    let after_marker = |start: usize| {
        let rest = &url[start..];
        let end = start + rest.find('/').unwrap_or(rest.len());
        (&url[..end], &url[end..])
    };
    if let Some(i) = url.find("://") {
        after_marker(i + 3)
    } else if url.starts_with("//") {
        after_marker(2)
    } else {
        ("", url)
    }
}

/// Resolve a reference against a base URL (RFC 3986 §5.2, minus dot-segment
/// removal — the container resolvers collapse `.`/`..` downstream, and a remote
/// result is never turned into a path). `base` may itself be relative, in which
/// case the result stays relative to the document and the ordinary container
/// resolution still applies.
fn resolve_against_base(base: &str, href: &str) -> String {
    let href = href.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0C'));
    if href.is_empty() {
        return base.to_string();
    }
    if url_scheme(href).is_some() {
        return href.to_string();
    }
    if let Some(frag) = href.strip_prefix('#') {
        let b = base.split('#').next().unwrap_or(base);
        return format!("{b}#{frag}");
    }
    let base = base.split('#').next().unwrap_or(base);
    if href.starts_with("//") {
        // Scheme-relative: inherits only the base's scheme.
        return match url_scheme(base) {
            Some(scheme) => format!("{scheme}:{href}"),
            None => href.to_string(),
        };
    }
    let base_no_query = base.split('?').next().unwrap_or(base);
    if let Some(rest) = href.strip_prefix('?') {
        return format!("{base_no_query}?{rest}");
    }
    let (origin, path) = split_url_authority(base_no_query);
    if href.starts_with('/') {
        return format!("{origin}{href}");
    }
    let merged = match path.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{href}"),
        // A base with an authority but no path merges as if its path were "/";
        // a relative base with no slash makes the reference its sibling.
        None if !origin.is_empty() => format!("/{href}"),
        None => href.to_string(),
    };
    format!("{origin}{merged}")
}

/// Resolve `href` against the directory of `source_path`. Returns `None`
/// for external URLs (`http:`, `mailto:`, …), pure fragments, and empty
/// hrefs. The result is the zip-relative path of the link target with the
/// fragment stripped.
fn resolve_href(source_path: &str, href: &str) -> Option<String> {
    // Leading/trailing ASCII whitespace is stripped from a URL before parsing
    // (WHATWG URL); a whitespace-only href (`href=" "`) is an empty same-document
    // reference, never a broken link.
    let href = href.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0C'));
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
    let mut parts: Vec<String> = if source_dir.is_empty() {
        Vec::new()
    } else {
        source_dir.split('/').map(String::from).collect()
    };
    // href segments are URL-encoded; decode each to match the literal zip names.
    for seg in no_frag.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                parts.pop();
            }
            other => parts.push(percent_decode(other)),
        }
    }
    Some(nfc(&parts.join("/")))
}

/// True when resolving `href` against `source_path`'s directory would rise above
/// the container root — more leading `..` than the source file's depth — which is
/// RSC-026 (leaks outside the container). Resolving to a *sibling* directory
/// inside the container (`../images/x.png`, or a resource at the zip root) is
/// legal and not flagged; only escaping the container root is.
fn href_leaks_container(source_path: &str, href: &str) -> bool {
    let no_frag = href.split('#').next().unwrap_or(href);
    let path = no_frag.split('?').next().unwrap_or(no_frag);
    let source_dir = source_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let mut depth = if source_dir.is_empty() {
        0
    } else {
        source_dir.split('/').count()
    };
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if depth == 0 {
                    return true;
                }
                depth -= 1;
            }
            _ => depth += 1,
        }
    }
    false
}

fn is_xhtml(media_type: &str) -> bool {
    media_type.eq_ignore_ascii_case("application/xhtml+xml")
}

fn read_text(zip: &mut ZipArchive<Cursor<&[u8]>>, name: &str) -> io::Result<String> {
    let idx = resolve_entry_index(zip, name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, name.to_owned()))?;
    let mut entry = zip.by_index(idx)?;
    let mut s = String::new();
    entry.read_to_string(&mut s)?;
    Ok(s)
}

fn read_bytes(zip: &mut ZipArchive<Cursor<&[u8]>>, name: &str) -> io::Result<Vec<u8>> {
    let idx = resolve_entry_index(zip, name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, name.to_owned()))?;
    let mut entry = zip.by_index(idx)?;
    let mut b = Vec::new();
    entry.read_to_end(&mut b)?;
    Ok(b)
}

/// The archive index of the entry whose OCF (UTF-8) path is `name`. The zip crate
/// keys `by_name`/`index_for_name` on the CP437-decoded name when an entry lacks
/// the language-encoding flag, so a non-ASCII OCF path (always UTF-8 per the spec)
/// can miss; fall back to matching the raw name bytes decoded as UTF-8, matching
/// how epubcheck reads OCF names.
fn resolve_entry_index(zip: &mut ZipArchive<Cursor<&[u8]>>, name: &str) -> Option<usize> {
    if let Some(i) = zip.index_for_name(name) {
        return Some(i);
    }
    (0..zip.len()).find(|&i| {
        zip.by_index(i)
            .is_ok_and(|f| nfc(&String::from_utf8_lossy(f.name_raw())) == name)
    })
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
        assert_eq!(Rule::ALL.len(), 103, "update Rule::ALL when adding a Rule");
    }

    #[test]
    fn has_errors_and_errors_display_key_on_error_severity() {
        use crate::validate::Severity;

        // A warning-only report is NOT `has_errors` — epubcheck exits 0 on
        // warnings, so this is the "would epubcheck reject it?" predicate the
        // conversion/repair flags depend on. `is_clean` (any violation) differs.
        let mut report = Report::default();
        report.push(Violation::new(
            Rule::DateSyntaxNotRecommended,
            "OPF",
            "date not W3C-DTF",
        ));
        assert!(!report.is_clean(), "a warning is still a violation");
        assert!(!report.has_errors(), "a warning must not read as an error");
        assert_eq!(report.count(Severity::Warning), 1);
        assert_eq!(report.count(Severity::Error), 0);
        assert!(
            report.errors_display().to_string().is_empty(),
            "no errors → empty error view"
        );

        // Add an error → the predicate flips and the error view shows only the
        // error line (the warning is filtered out).
        report.push(Violation::new(
            Rule::EmptyTitle,
            "ch1.xhtml",
            "empty <title>",
        ));
        assert!(report.has_errors());
        assert_eq!(report.count(Severity::Error), 1);
        let shown = report.errors_display().to_string();
        assert!(
            shown.contains(Rule::EmptyTitle.message_id()),
            "error line present: {shown:?}"
        );
        assert!(
            !shown.contains(Rule::DateSyntaxNotRecommended.message_id()),
            "warning line filtered out: {shown:?}"
        );
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
        // HTM-001/003 + RSC-013; container/URL batch RSC-003 + RSC-033;
        // cross-doc batch NCX-001 + RSC-008 + OPF-073 + OPF-032; encoding batch
        // RSC-028 + HTM-058 (RSC-027 is a warning, not counted here); fallback/
        // rootfile batch OPF-016/017/043/044/045; metadata batch OPF-054 (the
        // date's EPUB-2 error id — OPF-053/085 are warnings, RSC-005 for missing
        // title/language was already counted); language batch OPF-092; remote/data
        // batch RSC-006 (remote spine item) + RSC-029 (manifest data: URL); OPF
        // <link> batch OPF-089 (alternate+other) + OPF-095 (voicing not audio) +
        // OPF-098 (link into package doc) + OPF-067 (link to non-spine item);
        // hyperlink-target batch RSC-010 (target not a content doc) + RSC-011
        // (target not a spine item); nav batch NAV-010 (nav links remote);
        // obfuscation batch PKG-026 (obfuscated resource not a font); bitmap batch
        // OPF-029 (image header vs declared media-type); recall-gap batch OPF-014
        // (undeclared manifest property) + OPF-028 (undeclared vocab prefix) +
        // HTM-046 (fixed-layout doc without a viewport meta) + PKG-021 (undecodable
        // raster image) + RSC-020 (invalid reference URL); fixed-layout/stylesheet
        // batch HTM-047/056/057/059 (viewport metadata) + HTM-048 (SVG viewBox) +
        // CSS-015 (untitled alternate stylesheet) + CSS-002 (empty url()) +
        // RSC-015 (svg use without a fragment) + OPF-041 (missing fallback-style).
        assert_eq!(
            covered, 76,
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
    fn collect_ncx_content_srcs_reads_src_values() {
        let ncx = r#"<ncx><navMap>
            <navPoint><content src="a.xhtml#f1"/></navPoint>
            <navPoint><content src="b.xhtml"/></navPoint>
        </navMap></ncx>"#;
        assert_eq!(
            collect_ncx_content_srcs(ncx),
            vec!["a.xhtml#f1".to_string(), "b.xhtml".to_string()],
        );
    }

    #[test]
    fn mimetype_content_check_matches_epubcheck() {
        // epubcheck's PKG-007 is content-only, read through the zip (decompressed,
        // central-directory size). So: (1) a correct STORED mimetype is clean;
        // (2) a correct mimetype STORED with a data descriptor (GP bit 3 → zeroed
        // local-header size) is clean, not a false PKG-007; (3) a Deflated
        // mimetype is left alone (compression is not an epubcheck error); (4) a
        // STORED mimetype with wrong content still fires.
        fn hdr(flags: u16, comp: u16, uncomp: u32, data: &[u8]) -> Vec<u8> {
            let name = b"mimetype";
            let mut v = Vec::new();
            v.extend_from_slice(b"PK\x03\x04");
            v.extend_from_slice(&0u16.to_le_bytes()); // version needed
            v.extend_from_slice(&flags.to_le_bytes());
            v.extend_from_slice(&comp.to_le_bytes());
            v.extend_from_slice(&0u32.to_le_bytes()); // mod time/date
            v.extend_from_slice(&0u32.to_le_bytes()); // crc32
            v.extend_from_slice(&(data.len() as u32).to_le_bytes()); // comp size
            v.extend_from_slice(&uncomp.to_le_bytes()); // uncomp size
            v.extend_from_slice(&(name.len() as u16).to_le_bytes());
            v.extend_from_slice(&0u16.to_le_bytes()); // extra len
            v.extend_from_slice(name);
            v.extend_from_slice(data);
            v.extend_from_slice(&[0u8; 40]); // pad past the min-size guard
            v
        }
        const REQ: &[u8] = b"application/epub+zip";
        let fires = |bytes: &[u8]| {
            let mut r = Report::default();
            check_mimetype_header(bytes, &mut r);
            r.has_rule(Rule::MimetypeBadContent)
        };
        assert!(!fires(&hdr(0, 0, REQ.len() as u32, REQ)), "stored+correct");
        assert!(
            !fires(&hdr(0x08, 0, 0, REQ)),
            "stored+data-descriptor+correct"
        );
        assert!(!fires(&hdr(0, 8, 5, b"\x00\x01\x02\x03\x04")), "deflated");
        assert!(fires(&hdr(0, 0, 14, b"application/xxx")), "stored+wrong");
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
    fn base_url_resolution_follows_the_declared_base() {
        // RFC 3986 §5.2 merge, over both absolute and document-relative bases.
        let cases: &[(&str, &str, &str)] = &[
            (
                "http://example.org/",
                "style.css",
                "http://example.org/style.css",
            ),
            (
                "http://example.org",
                "style.css",
                "http://example.org/style.css",
            ),
            (
                "http://example.org/a/b.html",
                "c.png",
                "http://example.org/a/c.png",
            ),
            (
                "http://example.org/a/b.html",
                "/c.png",
                "http://example.org/c.png",
            ),
            (
                "http://example.org/a/b.html",
                "#frag",
                "http://example.org/a/b.html#frag",
            ),
            ("http://example.org/a/?q=1", "//other/x", "http://other/x"),
            ("http://example.org/a/b.html", "https://x/y", "https://x/y"),
            // A relative base keeps the reference relative to the document.
            ("css/", "style.css", "css/style.css"),
            ("./", "style.css", "./style.css"),
            ("other.html", "x.png", "x.png"),
            ("other.html", "#frag", "other.html#frag"),
            ("../a/b.html", "c.png", "../a/c.png"),
        ];
        for (base, href, want) in cases {
            assert_eq!(&resolve_against_base(base, href), want, "{base} + {href}");
        }
    }

    #[test]
    fn references_resolve_against_an_html_base_or_xml_base() {
        // An HTML <base href> applies from where it appears, in document order.
        let refs = collect_references(
            r##"<html><head>
                <link rel="stylesheet" href="before.css"/>
                <base href="http://example.org/"/>
                <link rel="stylesheet" href="after.css"/>
                </head><body><a href="#frag">x</a></body></html>"##,
        );
        let hrefs: Vec<&str> = refs.iter().map(|(_, h)| h.as_str()).collect();
        assert_eq!(
            hrefs,
            [
                "before.css",
                "http://example.org/after.css",
                "http://example.org/#frag"
            ]
        );
        // `xml:base` on an ancestor does the same, and is never restored on the
        // element's end (epubcheck's BaseURLHandler keeps one non-scoped value).
        let refs = typed_content_refs(
            r##"<html xml:base="http://example.org/"><body>
                <p><a href="file.html"/></p><img src="pic.png"/>
                </body></html>"##,
        );
        let hrefs: Vec<&str> = refs.iter().map(|(_, h)| h.as_str()).collect();
        assert_eq!(
            hrefs,
            ["http://example.org/file.html", "http://example.org/pic.png"]
        );
        // No base declaration → hrefs are handed back untouched.
        let refs = collect_references(r#"<html><body><img src="a/b.png"/></body></html>"#);
        assert_eq!(refs[0].1, "a/b.png");
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
    fn typed_content_refs_classify_for_remote_rule() {
        let refs = typed_content_refs(
            r##"<html><body>
                <a href="a.xhtml">x</a>
                <link rel="stylesheet" href="s.css"/>
                <link rel="prefetch" href="p.xhtml"/>
                <img src="i.jpg"/>
                <audio src="au.mp3"></audio>
                <video src="v.mp4" poster="po.jpg"></video>
                <picture><source src="ps.webp"/><img src="pi.jpg"/></picture>
                <audio><source src="as.mp3"/></audio>
                <video><source src="vs.mp4"/></video>
            </body></html>"##,
        );
        let ty = |h: &str| refs.iter().find(|(_, x)| x == h).map(|(t, _)| *t);
        // <a> and non-stylesheet <link> are link-like (always allowed remote).
        assert_eq!(ty("a.xhtml"), Some(RefType::LinkLike));
        assert_eq!(ty("p.xhtml"), Some(RefType::LinkLike));
        // A stylesheet <link> is not link-like.
        assert_eq!(ty("s.css"), Some(RefType::Other));
        // Media elements type by their own tag; a <source> types by its parent.
        assert_eq!(ty("au.mp3"), Some(RefType::Audio));
        assert_eq!(ty("as.mp3"), Some(RefType::Audio));
        assert_eq!(ty("v.mp4"), Some(RefType::Video));
        assert_eq!(ty("vs.mp4"), Some(RefType::Video));
        // A <video poster> and a <source> inside <picture> are images (Other).
        assert_eq!(ty("po.jpg"), Some(RefType::Other));
        assert_eq!(ty("ps.webp"), Some(RefType::Other));
        assert_eq!(ty("i.jpg"), Some(RefType::Other));
        assert_eq!(ty("pi.jpg"), Some(RefType::Other));
    }

    #[test]
    fn core_media_type_set_matches_epubcheck() {
        for core in [
            "audio/mpeg",
            "audio/mp4",
            "audio/ogg; codecs=opus",
            "video/anything",
            "font/woff2",
            "application/vnd.ms-opentype",
            "application/xhtml+xml",
            "image/svg+xml",
            "image/gif",
            "image/png",
            "image/jpeg",
            "image/webp",
            "text/javascript",
            "text/css",
            "application/pls+xml",
            "application/smil+xml",
        ] {
            assert!(is_core_media_type(core), "{core} should be core");
        }
        for foreign in [
            "image/vnd.xyz",
            "application/x-demo-slideshow2",
            "audio/foreign",
            "application/vnd.epubcheck",
        ] {
            assert!(!is_core_media_type(foreign), "{foreign} should be foreign");
        }
        // Only gif/png/jpeg/svg/webp are blessed *image* types (MED-003).
        assert!(is_blessed_image_type("image/png"));
        assert!(!is_blessed_image_type("image/vnd.xyz"));
    }

    #[test]
    fn palpable_content_excludes_hidden_and_metadata() {
        // Embedded content is always palpable; a hidden element never is; a
        // generic element counts iff it accumulated palpable content.
        assert!(is_palpable_elem(b"img", false, false));
        assert!(is_palpable_elem(b"object", false, false));
        assert!(!is_palpable_elem(b"p", true, true)); // hidden overrides content
        assert!(!is_palpable_elem(b"script", false, true));
        assert!(!is_palpable_elem(b"head", false, true));
        assert!(is_palpable_elem(b"p", false, true));
        assert!(!is_palpable_elem(b"p", false, false));
    }

    #[test]
    fn data_url_media_type_is_extracted() {
        assert_eq!(
            data_url_media_type("data:image/x-foreign;base64,AAAA"),
            Some("image/x-foreign")
        );
        assert_eq!(
            data_url_media_type("data:image/png,AAAA"),
            Some("image/png")
        );
        assert_eq!(data_url_media_type("data:,hello"), None);
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
        check_opf_structure(&pkg, "", "content.opf", false, true, &mut report);
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
        check_opf_structure(&pkg, "", "content.opf", false, true, &mut report);
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

    #[test]
    fn parse_doctype_extracts_external_identifiers() {
        let d =
            parse_doctype(r#"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "x.dtd">"#).unwrap();
        assert_eq!(d.public_id.as_deref(), Some("-//W3C//DTD XHTML 1.1//EN"));
        assert_eq!(d.system_id.as_deref(), Some("x.dtd"));
        let s = parse_doctype("<!DOCTYPE html SYSTEM 'about:legacy-compat'>").unwrap();
        assert_eq!(s.public_id, None);
        assert_eq!(s.system_id.as_deref(), Some("about:legacy-compat"));
        let plain = parse_doctype("<!DOCTYPE html>").unwrap();
        assert_eq!(plain.public_id, None);
        assert_eq!(plain.system_id, None);
        assert!(parse_doctype("<html/>").is_none());
    }

    #[test]
    fn doctype_rules_html_fires_public_but_allows_legacy_compat() {
        // EPUB 3 HTM-004: XHTML with a public identifier is irregular.
        let mut r = Report::default();
        check_doctype_rules(
            r#"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "x.dtd"><html/>"#,
            "c.xhtml",
            "application/xhtml+xml",
            false,
            true,
            &mut r,
        );
        assert!(r.has_rule(Rule::IrregularDoctype));
        // The EPUB-3 `about:legacy-compat` form must NOT fire (regression guard).
        let mut r2 = Report::default();
        check_doctype_rules(
            r#"<!DOCTYPE html SYSTEM "about:legacy-compat"><html/>"#,
            "c.xhtml",
            "application/xhtml+xml",
            false,
            true,
            &mut r2,
        );
        assert!(
            !r2.has_rule(Rule::IrregularDoctype),
            "legacy-compat is allowed"
        );
        // Plain `<!DOCTYPE html>` is allowed in EPUB 3.
        let mut r3 = Report::default();
        check_doctype_rules(
            "<!DOCTYPE html><html/>",
            "c.xhtml",
            "application/xhtml+xml",
            false,
            true,
            &mut r3,
        );
        assert!(!r3.has_rule(Rule::IrregularDoctype));
    }

    #[test]
    fn doctype_rules_epub2_requires_exact_content_dtd() {
        // EPUB 2 (OPS 2.0) XHTML must declare exactly the XHTML 1.1 DTD.
        let ok = r#"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd"><html/>"#;
        let mut r = Report::default();
        check_doctype_rules(ok, "c.xhtml", "application/xhtml+xml", true, false, &mut r);
        assert!(
            !r.has_rule(Rule::IrregularDoctype),
            "XHTML 1.1 is legal in EPUB 2"
        );
        // An HTML5 doctype, XHTML 1.0, and a mismatched system id all fire HTM-004
        // (these are the corpus's 16 gap books: empty/HTML5/XHTML-1.0 doctypes).
        for bad in [
            "<!DOCTYPE html><html/>",
            r#"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd"><html/>"#,
            r#"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "wrong.dtd"><html/>"#,
        ] {
            let mut r = Report::default();
            check_doctype_rules(bad, "c.xhtml", "application/xhtml+xml", true, false, &mut r);
            assert!(
                r.has_rule(Rule::IrregularDoctype),
                "EPUB 2 irregular DOCTYPE {bad:?}"
            );
        }
        // No DOCTYPE at all → nothing to check (epubcheck inspects only a present one).
        let mut r = Report::default();
        check_doctype_rules(
            "<html/>",
            "c.xhtml",
            "application/xhtml+xml",
            true,
            false,
            &mut r,
        );
        assert!(r.is_clean());
        // The EPUB 2 NCX must be exactly NCX 2005-1 (clean form).
        let mut r = Report::default();
        check_doctype_rules(
            r#"<!DOCTYPE ncx PUBLIC "-//NISO//DTD ncx 2005-1//EN" "http://www.daisy.org/z3986/2005/ncx-2005-1.dtd"><ncx/>"#,
            "toc.ncx",
            "application/x-dtbncx+xml",
            true,
            false,
            &mut r,
        );
        assert!(!r.has_rule(Rule::IrregularDoctype));
    }

    #[test]
    fn doctype_rules_opf073_is_dtd_and_version_gated() {
        // The sanctioned NCX DTD is allowed on an NCX resource in EPUB 3.
        let mut r = Report::default();
        check_doctype_rules(
            r#"<!DOCTYPE ncx PUBLIC "-//NISO//DTD ncx 2005-1//EN" "http://www.daisy.org/z3986/2005/ncx-2005-1.dtd"><ncx/>"#,
            "toc.ncx",
            "application/x-dtbncx+xml",
            false,
            true,
            &mut r,
        );
        assert!(!r.has_rule(Rule::DoctypeExternalIdentifier));
        // An arbitrary external id on the OPF is OPF-073 in EPUB 3.
        let mut r2 = Report::default();
        check_doctype_rules(
            r#"<!DOCTYPE package SYSTEM "oeb.dtd"><package/>"#,
            "content.opf",
            "application/oebps-package+xml",
            false,
            true,
            &mut r2,
        );
        assert!(r2.has_rule(Rule::DoctypeExternalIdentifier));
        // ...but not for a version-less package (rejected via OPF-001 instead);
        // an OPF DTD in EPUB 2 is HTM-009, which is not ported (so also silent).
        let mut r3 = Report::default();
        check_doctype_rules(
            r#"<!DOCTYPE package SYSTEM "oeb.dtd"><package/>"#,
            "content.opf",
            "application/oebps-package+xml",
            false,
            false,
            &mut r3,
        );
        assert!(!r3.has_rule(Rule::DoctypeExternalIdentifier));
    }

    #[test]
    fn content_document_classification_is_version_aware() {
        assert!(is_content_document("application/xhtml+xml", false));
        assert!(is_content_document("image/svg+xml", false)); // EPUB3 blessed
        assert!(!is_content_document("image/svg+xml", true)); // not in EPUB2
        assert!(is_content_document("application/x-dtbook+xml", true)); // EPUB2 blessed
        assert!(!is_content_document("image/jpeg", false));
        assert!(!is_content_document("text/css", false));
    }

    #[test]
    fn sniff_xml_encoding_reads_declaration_and_boms() {
        assert_eq!(
            sniff_xml_encoding(br#"<?xml version="1.0" encoding="UTF-8"?>xx"#).as_deref(),
            Some("UTF-8")
        );
        assert_eq!(
            sniff_xml_encoding(br#"<?xml version="1.0" encoding="iso-8859-1"?>xx"#).as_deref(),
            Some("ISO-8859-1")
        );
        // No declaration → None (assumed UTF-8).
        assert_eq!(sniff_xml_encoding(b"<html>hello there</html>"), None);
        // UTF-8 BOM.
        assert_eq!(
            sniff_xml_encoding(&[0xEF, 0xBB, 0xBF, b'<', b'x', b'/', b'>']).as_deref(),
            Some("UTF-8")
        );
        // UTF-16 LE BOM.
        assert_eq!(
            sniff_xml_encoding(&[0xFF, 0xFE, 0x3C, 0x00]).as_deref(),
            Some("UTF-16")
        );
        // Fewer than 4 bytes → None.
        assert_eq!(sniff_xml_encoding(b"<?"), None);
    }

    #[test]
    fn xml_encoding_check_flags_non_utf8_and_utf16() {
        // RSC-028: a declared legacy charset (error).
        let mut r = Report::default();
        check_xml_encoding(
            br#"<?xml version="1.0" encoding="Shift_JIS"?><x/>"#,
            "x.xhtml",
            "application/xhtml+xml",
            &mut r,
        );
        assert!(r.has_rule(Rule::XmlEncodingNotUtf8));
        // HTM-058: UTF-16 in XHTML is an error.
        let mut r2 = Report::default();
        check_xml_encoding(
            &[0xFF, 0xFE, 0x3C, 0x00],
            "x.xhtml",
            "application/xhtml+xml",
            &mut r2,
        );
        assert!(r2.has_rule(Rule::XhtmlEncodingUtf16));
        // RSC-027: UTF-16 in a non-XHTML XML resource is a warning.
        let mut r3 = Report::default();
        check_xml_encoding(
            &[0xFF, 0xFE, 0x3C, 0x00],
            "toc.ncx",
            "application/x-dtbncx+xml",
            &mut r3,
        );
        assert!(r3.has_rule(Rule::XmlEncodingUtf16));
        assert_eq!(
            r3.violations[0].rule.severity(),
            crate::validate::Severity::Warning
        );
        // A UTF-8 declaration (any case) produces no finding.
        let mut r4 = Report::default();
        check_xml_encoding(
            br#"<?xml version="1.0" encoding="utf-8"?><x/>"#,
            "x.xhtml",
            "application/xhtml+xml",
            &mut r4,
        );
        assert!(r4.is_clean());
    }

    #[test]
    fn ncx_dtb_uid_reads_meta_content() {
        let ncx = r#"<ncx><head><meta name="dtb:uid" content="urn:uuid:42"/></head></ncx>"#;
        assert_eq!(ncx_dtb_uid(ncx).as_deref(), Some("urn:uuid:42"));
        assert_eq!(ncx_dtb_uid("<ncx><head/></ncx>"), None);
    }

    #[test]
    fn guide_reference_to_non_content_doc_flags_opf032() {
        // A guide reference that IS in the manifest but points at an image is
        // OPF-032 (not OPF-031 — it is declared).
        let opf = r##"<package version="3.0" unique-identifier="id">
          <metadata><dc:identifier id="id">x</dc:identifier></metadata>
          <manifest>
            <item id="c" href="c.xhtml" media-type="application/xhtml+xml"/>
            <item id="img" href="cover.jpg" media-type="image/jpeg"/>
          </manifest>
          <spine><itemref idref="c"/></spine>
          <guide><reference type="cover" href="cover.jpg"/></guide>
        </package>"##;
        let pkg = opf::parse(opf).unwrap();
        let mut report = Report::default();
        check_opf_structure(&pkg, "", "content.opf", false, true, &mut report);
        assert!(report.has_rule(Rule::GuideReferenceNotContentDoc)); // OPF-032
        assert!(!report.has_rule(Rule::GuideReferenceNotInManifest)); // it is declared
    }

    #[test]
    fn spine_item_fallback_rules_flag_opf043_and_opf044() {
        let opf = r##"<package version="3.0" unique-identifier="id">
          <metadata><dc:identifier id="id">x</dc:identifier></metadata>
          <manifest>
            <item id="a" href="a.xml" media-type="application/x-weird+xml"/>
            <item id="b" href="b.bin" media-type="application/octet-stream" fallback="c"/>
            <item id="c" href="c.png" media-type="image/png"/>
            <item id="d" href="d.bin" media-type="application/octet-stream" fallback="x"/>
            <item id="x" href="x.xhtml" media-type="application/xhtml+xml"/>
          </manifest>
          <spine>
            <itemref idref="a"/>
            <itemref idref="b"/>
            <itemref idref="d"/>
          </spine>
        </package>"##;
        let pkg = opf::parse(opf).unwrap();
        let mut r = Report::default();
        check_fallback_chain_and_spine(&pkg, false, true, "content.opf", &mut r);
        // a: non-blessed, no fallback → OPF-043. b: fallback→png (not content)
        // → OPF-044. d: fallback→xhtml reaches a content document → clean.
        let n43 = r
            .violations
            .iter()
            .filter(|v| v.rule == Rule::SpineItemNoFallback)
            .count();
        let n44 = r
            .violations
            .iter()
            .filter(|v| v.rule == Rule::SpineItemFallbackNotContentDoc)
            .count();
        assert_eq!((n43, n44), (1, 1), "got:\n{r}");
    }

    #[test]
    fn fallback_cycle_flags_opf045_once() {
        // `main` (the spine item) is blessed; a↔b form a fallback cycle that is
        // not in the spine, so only OPF-045 fires — exactly once.
        let opf = r##"<package version="3.0" unique-identifier="id">
          <metadata><dc:identifier id="id">x</dc:identifier></metadata>
          <manifest>
            <item id="main" href="m.xhtml" media-type="application/xhtml+xml"/>
            <item id="a" href="a.xml" media-type="application/x-a+xml" fallback="b"/>
            <item id="b" href="b.xml" media-type="application/x-b+xml" fallback="a"/>
          </manifest>
          <spine><itemref idref="main"/></spine>
        </package>"##;
        let pkg = opf::parse(opf).unwrap();
        let mut r = Report::default();
        check_fallback_chain_and_spine(&pkg, false, true, "content.opf", &mut r);
        assert_eq!(
            r.violations
                .iter()
                .filter(|v| v.rule == Rule::FallbackChainCircular)
                .count(),
            1,
            "OPF-045 reported once, got:\n{r}"
        );
    }

    #[test]
    fn w3c_date_parser_accepts_valid_and_rejects_malformed() {
        // Every W3C-DTF granularity the spec permits — all must parse.
        for ok in [
            "2015",
            "2015-08",
            "2015-08-05",
            "2015-08-05T22:00:00Z",
            "2015-08-05T22:00:00+00:00",
            "2015-08-05T22:00:00-05:30",
            "2015-08-05T22:00:00.5Z",
            "2016-02-29", // leap day
        ] {
            assert!(parse_w3c_date(ok).is_ok(), "should accept {ok:?}");
        }
        // Clearly malformed / out-of-range — all must be rejected.
        for bad in [
            "",
            "2015-",
            "2015-13-01",           // month 13
            "2015-08-32",           // day 32
            "2015-02-30",           // Feb 30
            "2015/08/05",           // wrong delimiter
            "20150805",             // 8-digit "year"
            "2015-08-05T25:00:00Z", // hour 25
            "2015-08-05 22:00",     // space is not a delimiter
        ] {
            assert!(parse_w3c_date(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn uuid_syntax_validation() {
        assert!(is_valid_uuid("f47ac10b-58cc-4372-a567-0e02b2c3d479"));
        assert!(is_valid_uuid("F47AC10B-58CC-4372-A567-0E02B2C3D479")); // case-insensitive
        assert!(!is_valid_uuid("not-a-uuid"));
        assert!(!is_valid_uuid("f47ac10b58cc4372a5670e02b2c3d479")); // no groups
        assert!(!is_valid_uuid("f47ac10b-58cc-4372-a567-0e02b2c3d47")); // group too short
        assert!(!is_valid_uuid("g47ac10b-58cc-4372-a567-0e02b2c3d479")); // non-hex
    }

    #[test]
    fn language_tag_wellformedness_matches_java_locale_builder() {
        // Well-formed (Java's Locale.Builder().setLanguageTag accepts): syntax
        // only, so unregistered-but-syntactic tags pass too.
        for ok in [
            "en",
            "zh",
            "ja",
            "en-US",
            "zh-Hans",
            "zh-Hans-CN",
            "de-CH-1901",         // variant
            "sl-rozaj-biske",     // two variants
            "de-DE-u-co-phonebk", // BCP-47 'u' extension
            "en-a-bbb-x-a-ccc",   // extension + private use
            "x-klingon",          // private-use only
            "english",            // 7 ALPHA: well-formed, though not a real code
            "zz",                 // syntactically fine, not registered
            "i-klingon",          // grandfathered (irregular)
            "en-GB-oed",          // grandfathered (irregular)
            "art-lojban",         // grandfathered (regular)
            "zh-min-nan",         // grandfathered (regular)
        ] {
            assert!(
                language_tag_wellformed(ok).is_ok(),
                "should accept {ok:?}: {:?}",
                language_tag_wellformed(ok)
            );
        }
        // Ill-formed: Java throws IllformedLocaleException.
        for bad in [
            "en_US",         // underscore is not a BCP-47 separator
            "en-",           // trailing separator → empty subtag
            "-en",           // leading separator → empty subtag
            "en--US",        // doubled separator → empty subtag
            "toolongsubtag", // 13 ALPHA: no production accepts it
            "a",             // single char: not language, not private-use prefix
            "en-a",          // singleton with no extension subtag
            "en-a-x-y",      // singleton 'a' unfollowed by a 2*8 subtag
            "x",             // private-use prefix with no subtag
            "de-419-DE-1a",  // '1a' is neither variant nor anything downstream
        ] {
            assert!(
                language_tag_wellformed(bad).is_err(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn metadata_check_flags_an_illformed_language_tag() {
        let opf = r##"<package version="3.0" unique-identifier="uid">
          <metadata xmlns:opf="http://www.idpf.org/2007/opf">
            <dc:title>Title</dc:title>
            <dc:identifier id="uid">urn:uuid:f47ac10b-58cc-4372-a567-0e02b2c3d479</dc:identifier>
            <dc:language>en_US</dc:language>
          </metadata>
          <manifest><item id="a" href="a.xhtml" media-type="application/xhtml+xml"/></manifest>
          <spine><itemref idref="a"/></spine>
        </package>"##;
        let pkg = opf::parse(opf).unwrap();
        let mut r = Report::default();
        check_metadata(&pkg, false, "content.opf", &mut r);
        assert!(r.has_rule(Rule::LanguageTagNotWellFormed));
        // The tag is present, so the missing-language rule must NOT also fire.
        assert!(!r.has_rule(Rule::MissingLanguage));
    }

    #[test]
    fn metadata_check_flags_missing_title_language_and_bad_values() {
        // Missing dc:title and dc:language, an invalid date, and a bogus UUID.
        let opf = r##"<package version="3.0" unique-identifier="uid">
          <metadata xmlns:opf="http://www.idpf.org/2007/opf">
            <dc:identifier id="uid" opf:scheme="uuid">urn:uuid:not-a-uuid</dc:identifier>
            <dc:date>2015-13-99</dc:date>
          </metadata>
          <manifest><item id="a" href="a.xhtml" media-type="application/xhtml+xml"/></manifest>
          <spine><itemref idref="a"/></spine>
        </package>"##;
        let pkg = opf::parse(opf).unwrap();
        let mut r = Report::default();
        check_metadata(&pkg, false, "content.opf", &mut r);
        assert!(r.has_rule(Rule::MissingTitle));
        assert!(r.has_rule(Rule::MissingLanguage));
        assert!(r.has_rule(Rule::IdentifierInvalidUuid));
        assert!(r.has_rule(Rule::DateSyntaxNotRecommended)); // EPUB 3 → OPF-053 warning
    }

    #[test]
    fn metadata_check_is_clean_for_a_well_formed_package() {
        // Present title/language, a valid ISO date, and a valid UUID: no findings.
        let opf = r##"<package version="3.0" unique-identifier="uid">
          <metadata xmlns:opf="http://www.idpf.org/2007/opf">
            <dc:title>Title</dc:title>
            <dc:language>en</dc:language>
            <dc:date>2015-08-05T22:00:00+00:00</dc:date>
            <dc:identifier id="uid">urn:uuid:f47ac10b-58cc-4372-a567-0e02b2c3d479</dc:identifier>
          </metadata>
          <manifest><item id="a" href="a.xhtml" media-type="application/xhtml+xml"/></manifest>
          <spine><itemref idref="a"/></spine>
        </package>"##;
        let pkg = opf::parse(opf).unwrap();
        let mut r = Report::default();
        check_metadata(&pkg, false, "content.opf", &mut r);
        assert!(
            r.is_clean(),
            "well-formed metadata should be clean, got:\n{r}"
        );
    }

    #[test]
    fn undeclared_prefix_flags_only_unreserved_undeclared() {
        // `access:` (itemref) and `kadokawa:` (meta) are undeclared; reserved
        // (`dcterms`, `schema`, `rendition`) and declared (`foaf`) prefixes, and
        // unprefixed tokens (`svg`, `page-spread-right`), must never fire.
        let opf = r##"<package version="3.0" unique-identifier="uid"
              prefix="foaf: http://xmlns.com/foaf/spec/">
          <metadata>
            <dc:title>T</dc:title>
            <dc:identifier id="uid">urn:uuid:1</dc:identifier>
            <meta property="dcterms:modified">2020-01-01T00:00:00Z</meta>
            <meta property="schema:accessMode">textual</meta>
            <meta property="foaf:name">X</meta>
            <meta property="kadokawa:version">1.0</meta>
          </metadata>
          <manifest>
            <item id="a" href="a.xhtml" media-type="application/xhtml+xml" properties="svg"/>
          </manifest>
          <spine>
            <itemref idref="a" properties="page-spread-right access:scroll-both"/>
          </spine>
        </package>"##;
        let pkg = opf::parse(opf).unwrap();
        let mut r = Report::default();
        check_prefix_declarations(&pkg, "content.opf", &mut r);
        assert!(
            r.violations
                .iter()
                .all(|v| v.rule == Rule::UndeclaredPrefix)
        );
        let msgs: Vec<&str> = r.violations.iter().map(|v| v.message.as_str()).collect();
        assert_eq!(
            r.violations.len(),
            2,
            "one per distinct undeclared prefix, got {msgs:?}"
        );
        assert!(msgs.iter().any(|m| m.contains("\"access\"")));
        assert!(msgs.iter().any(|m| m.contains("\"kadokawa\"")));
    }

    #[test]
    fn undeclared_prefix_clean_when_all_reserved_or_declared() {
        let opf = r##"<package version="3.0" unique-identifier="uid"
              prefix="ex: http://example.org/">
          <metadata>
            <dc:title>T</dc:title>
            <dc:identifier id="uid">urn:uuid:1</dc:identifier>
            <meta property="dcterms:modified">2020-01-01T00:00:00Z</meta>
            <meta property="ex:foo">y</meta>
          </metadata>
          <manifest>
            <item id="a" href="a.xhtml" media-type="application/xhtml+xml" properties="svg nav"/>
          </manifest>
          <spine><itemref idref="a" properties="page-spread-right rendition:layout-reflowable"/></spine>
        </package>"##;
        let pkg = opf::parse(opf).unwrap();
        let mut r = Report::default();
        check_prefix_declarations(&pkg, "content.opf", &mut r);
        assert!(r.is_clean(), "no undeclared prefixes; got:\n{r}");
    }

    #[test]
    fn detect_content_features_covers_the_required_property_signals() {
        // Inline SVG + a viewport meta; no math/script/form/remote.
        let svg = r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>t</title>
          <meta name="viewport" content="width=1200,height=1600"/></head>
          <body><div><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1 1"><rect/></svg></div></body></html>"#;
        let f = detect_content_features(svg);
        assert!(f.inline_svg && f.has_viewport);
        assert!(!f.mathml && !f.scripted && !f.remote_resources);

        // A form (→ scripted), MathML, and a remote <img>. No inline svg/viewport.
        let mixed = r#"<html><head><title>t</title></head><body>
          <form action="x"></form>
          <math xmlns="http://www.w3.org/1998/Math/MathML"><mn>1</mn></math>
          <img src="https://cdn.example.com/a.png"/>
        </body></html>"#;
        let f = detect_content_features(mixed);
        assert!(f.scripted && f.mathml && f.remote_resources);
        assert!(!f.inline_svg && !f.has_viewport);

        // A JSON data block is not scripting; a typeless <script> is.
        assert!(
            !detect_content_features(
                r#"<html><body><script type="application/ld+json">{}</script></body></html>"#
            )
            .scripted
        );
        assert!(
            detect_content_features(r#"<html><body><script>var x=1;</script></body></html>"#)
                .scripted
        );
        // A *local* resource is not a remote resource.
        assert!(
            !detect_content_features(r#"<html><body><img src="../i/x.png"/></body></html>"#)
                .remote_resources
        );
    }

    #[test]
    fn fixed_layout_resolution_matches_epubcheck() {
        let build = |global: &str| {
            format!(
                r##"<package version="3.0" unique-identifier="u">
              <metadata><dc:title>t</dc:title><dc:identifier id="u">urn:uuid:1</dc:identifier>
                <meta property="rendition:layout">{global}</meta></metadata>
              <manifest>
                <item id="f" href="f.xhtml" media-type="application/xhtml+xml"/>
                <item id="r" href="r.xhtml" media-type="application/xhtml+xml"/>
                <item id="d" href="d.xhtml" media-type="application/xhtml+xml"/>
              </manifest>
              <spine>
                <itemref idref="f" properties="rendition:layout-pre-paginated"/>
                <itemref idref="r" properties="rendition:layout-reflowable"/>
                <itemref idref="d"/>
              </spine>
            </package>"##
            )
        };
        // Global reflowable: only the explicit pre-paginated override is fixed.
        let pkg = opf::parse(&build("reflowable")).unwrap();
        let ids = fixed_layout_spine_ids(&pkg);
        assert!(ids.contains("f") && !ids.contains("r") && !ids.contains("d"));
        // Global pre-paginated: the un-annotated item inherits fixed; the explicit
        // reflowable override stays reflowable.
        let pkg = opf::parse(&build("pre-paginated")).unwrap();
        let ids = fixed_layout_spine_ids(&pkg);
        assert!(ids.contains("f") && ids.contains("d") && !ids.contains("r"));
    }

    #[test]
    fn fixed_layout_svg_epub_flags_opf014_and_htm046_end_to_end() {
        // Publication default is fixed-layout. p1: inline SVG, no viewport → OPF-014
        // (svg) + HTM-046. p2: viewport present, no svg → clean. nav: viewport.
        let opf = r##"<package version="3.0" unique-identifier="u">
          <metadata><dc:title>t</dc:title><dc:language>en</dc:language>
            <dc:identifier id="u">urn:uuid:f47ac10b-58cc-4372-a567-0e02b2c3d479</dc:identifier>
            <meta property="dcterms:modified">2020-01-01T00:00:00Z</meta>
            <meta property="rendition:layout">pre-paginated</meta></metadata>
          <manifest>
            <item id="p1" href="p1.xhtml" media-type="application/xhtml+xml"/>
            <item id="p2" href="p2.xhtml" media-type="application/xhtml+xml"/>
            <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
          </manifest>
          <spine><itemref idref="p1"/><itemref idref="p2"/><itemref idref="nav"/></spine>
        </package>"##;
        let vp = r#"<meta name="viewport" content="width=1200, height=1600"/>"#;
        let p1 = r#"<?xml version="1.0" encoding="utf-8"?><!DOCTYPE html>
          <html xmlns="http://www.w3.org/1999/xhtml"><head><title>p1</title></head>
          <body><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><rect width="100" height="100"/></svg></body></html>"#;
        let p2 = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><!DOCTYPE html>
          <html xmlns="http://www.w3.org/1999/xhtml"><head><title>p2</title>{vp}</head>
          <body><p>text</p></body></html>"#
        );
        let nav = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><!DOCTYPE html>
          <html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
          <head><title>nav</title>{vp}</head>
          <body><nav epub:type="toc"><ol><li><a href="p1.xhtml">p1</a></li></ol></nav></body></html>"#
        );
        let bytes = zip_epub(
            opf,
            &[("p1.xhtml", p1), ("p2.xhtml", &p2), ("nav.xhtml", &nav)],
        );
        let report = validate(&bytes);
        let opf014 = report
            .violations
            .iter()
            .filter(|v| v.rule == Rule::PropertyNotDeclared)
            .count();
        let htm046 = report
            .violations
            .iter()
            .filter(|v| v.rule == Rule::FixedLayoutNoViewport)
            .count();
        assert_eq!(
            opf014, 1,
            "exactly p1 needs an undeclared svg; got:\n{report}"
        );
        assert_eq!(htm046, 1, "exactly p1 lacks a viewport; got:\n{report}");
    }

    #[test]
    fn invalid_url_flags_raw_space_in_opf_hrefs() {
        // A manifest item href with a raw space is RSC-020; a %20-encoded one and
        // a data: URL (whitespace stripped by epubcheck) are not.
        let opf = r##"<package version="3.0" unique-identifier="u">
          <metadata><dc:title>t</dc:title><dc:identifier id="u">urn:uuid:1</dc:identifier></metadata>
          <manifest>
            <item id="a" href="Text/Philip K Dick.html" media-type="application/xhtml+xml"/>
            <item id="b" href="Text/ok%20name.xhtml" media-type="application/xhtml+xml"/>
            <item id="c" href="c.xhtml" media-type="application/xhtml+xml"/>
          </manifest>
          <spine><itemref idref="c"/></spine>
        </package>"##;
        let pkg = opf::parse(opf).unwrap();
        let mut r = Report::default();
        check_opf_url_validity(&pkg, "content.opf", &mut r);
        assert!(r.violations.iter().all(|v| v.rule == Rule::InvalidUrl));
        assert_eq!(r.violations.len(), 1, "only the raw-space href; got:\n{r}");
        assert!(r.violations[0].message.contains("Philip K Dick"));

        // Position matters: a space only in the query/fragment is not judged
        // (epubcheck's path/host reasons), and data: URLs are exempt.
        assert!(url_has_illegal_space("a b.xhtml"));
        assert!(url_has_illegal_space("http://ex ample.org/x"));
        assert!(!url_has_illegal_space("ok.xhtml?q=a b"));
        assert!(!url_has_illegal_space("ok.xhtml#a b"));
        assert!(!url_has_illegal_space("data:text/plain, a b"));
        // The URL parser strips leading/trailing space and removes tab/newline
        // before parsing, so those are never RSC-020 (regression: a trailing space
        // on `http://www.ylib.com ` was a real corpus false positive).
        assert!(!url_has_illegal_space("http://www.ylib.com "));
        assert!(!url_has_illegal_space("  ../style.css\n"));
        assert!(!url_has_illegal_space("a\tb.xhtml"));
    }

    #[test]
    fn well_formed_xml_passes_cleanly() {
        let mut r = Report::default();
        check_xml_well_formedness(
            r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>t</title></head><body><p>hi &amp; bye</p></body></html>"#,
            "x.xhtml",
            &mut r,
        );
        assert!(r.is_clean(), "well-formed XHTML should pass, got:\n{r}");
        // Comments/PIs/whitespace after the root element are allowed.
        let mut r = Report::default();
        check_xml_well_formedness("<r/>\n  <!-- ok -->\n", "x.xml", &mut r);
        assert!(
            r.is_clean(),
            "trailing whitespace/comment is allowed, got:\n{r}"
        );
    }

    #[test]
    fn malformed_xml_structural_classes_fire_rsc016() {
        // Each construct is a fatal parse error in epubcheck's XML parser; the
        // labels track the six corpus RSC-016 failure modes we detect.
        let cases: &[(&str, &str)] = &[
            ("mismatched end tag", "<a><b>x</a></b>"),
            ("unterminated element", "<html><body><p>x</p></body>"),
            ("content after root", "<r>x</r>trailing"),
            (
                "duplicate attribute",
                r#"<html lang="en" lang="fr">x</html>"#,
            ),
            (
                "unbound element prefix",
                r#"<opf:package xmlns="urn:x"><a/></opf:package>"#,
            ),
            (
                "unbound attribute prefix",
                r#"<package xmlns="urn:x"><id opf:scheme="ASIN">x</id></package>"#,
            ),
        ];
        for (label, xml) in cases {
            let mut r = Report::default();
            check_xml_well_formedness(xml, "x.xml", &mut r);
            assert!(
                r.has_rule(Rule::MalformedXml),
                "{label}: expected RSC-016 on {xml:?}, got:\n{r}"
            );
        }
    }

    #[test]
    fn opf_schema_attrs_flag_disallowed_attributes_by_version() {
        // Helper: run the check under an explicit (epub2, epub3) pair.
        let run = |xml: &str, epub2: bool, epub3: bool| {
            let mut r = Report::default();
            check_opf_schema(xml, "content.opf", epub2, epub3, &mut r);
            r
        };

        // EPUB 3: an OPF-namespaced attribute (`opf:role`/`opf:file-as`/
        // `opf:scheme`) on a `<dc:*>` element is a schema violation (RSC-005) —
        // whatever prefix the OPF namespace is bound to.
        let e3_role = r#"<package xmlns="http://www.idpf.org/2007/opf" xmlns:opf="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" version="3.0"><metadata><dc:creator opf:role="aut" opf:file-as="Doe, J">J. Doe</dc:creator><dc:identifier opf:scheme="uuid">u</dc:identifier></metadata></package>"#;
        assert!(
            run(e3_role, false, true).has_rule(Rule::DisallowedOpfAttribute),
            "EPUB 3 opf: metadata attribute must fire RSC-005"
        );
        // The same OPF namespace bound to a non-`opf` prefix is still caught
        // (resolution is by namespace URI, not prefix spelling).
        let e3_ns1 = r#"<package xmlns="http://www.idpf.org/2007/opf" xmlns:ns1="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" version="3.0"><metadata><dc:creator ns1:role="aut">x</dc:creator></metadata></package>"#;
        assert!(
            run(e3_ns1, false, true).has_rule(Rule::DisallowedOpfAttribute),
            "OPF namespace under a foreign prefix must still fire"
        );

        // EPUB 3 clean: no OPF-namespaced attributes. `xml:lang`, unprefixed
        // `id`, and `page-progression-direction` on the spine are all legal.
        let e3_clean = r#"<package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" version="3.0"><metadata><dc:creator id="c" xml:lang="en">x</dc:creator></metadata><spine toc="ncx" page-progression-direction="rtl"><itemref idref="a"/></spine></package>"#;
        assert!(
            run(e3_clean, false, true).is_clean(),
            "clean EPUB 3 OPF must not fire, got:\n{}",
            run(e3_clean, false, true)
        );

        // EPUB 2: the very same `opf:role`/`opf:file-as` are LEGAL — no finding.
        let e2_role = r#"<package xmlns="http://www.idpf.org/2007/opf" xmlns:opf="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" version="2.0"><metadata><dc:creator opf:role="aut" opf:file-as="Doe, J">J. Doe</dc:creator></metadata></package>"#;
        assert!(
            run(e2_role, true, false).is_clean(),
            "EPUB 2 opf: metadata attributes are legal, got:\n{}",
            run(e2_role, true, false)
        );

        // EPUB 2: `page-progression-direction` and `page-map` on `<spine>` are
        // EPUB-3/Adobe attributes the EPUB 2 spine attlist (`id`/`toc`) forbids.
        for spine in [
            r#"<spine toc="ncx" page-progression-direction="rtl"><itemref idref="a"/></spine>"#,
            r#"<spine toc="ncx" page-map="pm"><itemref idref="a"/></spine>"#,
        ] {
            let xml = format!(
                r#"<package xmlns="http://www.idpf.org/2007/opf" version="2.0"><metadata/>{spine}</package>"#
            );
            assert!(
                run(&xml, true, false).has_rule(Rule::DisallowedOpfAttribute),
                "EPUB 2 disallowed spine attribute must fire on {spine:?}"
            );
        }
        // EPUB 2 clean spine (only id/toc).
        let e2_spine_ok = r#"<package xmlns="http://www.idpf.org/2007/opf" version="2.0"><metadata/><spine id="s" toc="ncx"><itemref idref="a"/></spine></package>"#;
        assert!(
            run(e2_spine_ok, true, false).is_clean(),
            "EPUB 2 spine with only id/toc is clean, got:\n{}",
            run(e2_spine_ok, true, false)
        );

        // Version-less/legacy package: not schema-validated here (OPF-001 covers
        // it), so even an opf: attribute produces nothing.
        assert!(
            run(e3_role, false, false).is_clean(),
            "version-less package must not be schema-attr-checked"
        );

        // EPUB 2 `<item>`: the EPUB 3 `properties` attribute does not exist in
        // the 2.0 manifest grammar (the rest of the attlist is legal).
        let e2_item = |extra: &str| {
            format!(
                r#"<package xmlns="http://www.idpf.org/2007/opf" version="2.0"><metadata/><manifest><item id="c" href="c.jpg" media-type="image/jpeg"{extra}/></manifest></package>"#
            )
        };
        assert!(
            run(&e2_item(r#" properties="cover-image""#), true, false)
                .has_rule(Rule::DisallowedOpfAttribute),
            "EPUB 2 <item properties> must fire"
        );
        assert!(
            run(
                &e2_item(r#" fallback="x" required-namespace="urn:y""#),
                true,
                false
            )
            .is_clean(),
            "the EPUB 2 item attlist is legal"
        );

        // EPUB 2 `<dc:*>`: the OPF refinements are namespaced, so an unprefixed
        // `role` is a violation while `opf:role`/`xml:lang`/`xsi:type` are not.
        let e2_dc = |attrs: &str| {
            format!(
                r#"<package xmlns="http://www.idpf.org/2007/opf" xmlns:opf="http://www.idpf.org/2007/opf" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:dc="http://purl.org/dc/elements/1.1/" version="2.0"><metadata><dc:creator {attrs}>A</dc:creator></metadata></package>"#
            )
        };
        assert!(
            run(&e2_dc(r#"role="aut""#), true, false).has_rule(Rule::DisallowedOpfAttribute),
            "an unprefixed role on <dc:creator> must fire"
        );
        assert!(
            run(
                &e2_dc(r#"id="c" xml:lang="en" opf:role="aut" opf:file-as="A" xsi:type="x""#),
                true,
                false
            )
            .is_clean(),
            "the EPUB 2 dc attlist union is legal, got:\n{}",
            run(&e2_dc(r#"id="c" opf:role="aut""#), true, false)
        );

        // The package document's `id` attributes go through the same walk: the
        // NCName lexical form (both versions) and per-document uniqueness.
        let bad_ids = r#"<package xmlns="http://www.idpf.org/2007/opf" version="2.0"><metadata/><manifest><item id="1first" href="a.xhtml" media-type="application/xhtml+xml"/><item id="dup" href="b.xhtml" media-type="application/xhtml+xml"/><item id="dup" href="c.xhtml" media-type="application/xhtml+xml"/></manifest></package>"#;
        let r = run(bad_ids, true, false);
        assert!(
            r.has_rule(Rule::IdNotNcName),
            "leading digit is not an NCName"
        );
        assert!(r.has_rule(Rule::DuplicateId), "repeated id must fire");
    }

    #[test]
    fn viewport_microsyntax_matches_the_java_parser() {
        // Well-formed lists, including the whitespace/keyword/float forms the
        // epubcheck fixtures cover.
        let props = |s: &str| parse_viewport_meta(s).unwrap();
        assert_eq!(
            props("width=1200,height=1600"),
            [
                ("width".into(), "1200".into()),
                ("height".into(), "1600".into())
            ]
        );
        assert_eq!(
            props("  width = device-width ;  height=device-height  "),
            [
                ("width".into(), "device-width".into()),
                ("height".into(), "device-height".into())
            ]
        );
        assert_eq!(props("width=1200.5")[0].1, "1200.5");
        // Duplicates are kept in order — HTM-059 counts them.
        assert_eq!(props("width=100,width=200").len(), 2);
        // Parse errors: empty, a trailing separator, an empty value, a stray '='.
        for bad in ["", "width=", "width=1,", ",width=1", "width=1=2"] {
            assert!(parse_viewport_meta(bad).is_err(), "{bad:?} must not parse");
        }
        // A bare name is *not* a parse error — the Java parser finalizes it with an
        // empty value, which then fails the value check (HTM-057, not HTM-047).
        assert_eq!(props("width"), [("width".into(), String::new())]);
        assert!(!viewport_value_is_valid("width", ""));
        // Value validity: a number or the matching device-* keyword.
        for (p, v, ok) in [
            ("width", "1200", true),
            ("width", "1200.5", true),
            ("width", "device-width", true),
            ("width", "device-height", false),
            ("width", "1200px", false),
            ("height", "100%", false),
            ("height", "device-height", true),
            ("initial-scale", "1.0", true), // other properties are unconstrained
        ] {
            assert_eq!(viewport_value_is_valid(p, v), ok, "{p}={v}");
        }
    }

    #[test]
    fn fixed_layout_viewport_rules_report_each_defect() {
        let run = |content: &str| {
            let mut r = Report::default();
            check_viewport_meta(content, "c.xhtml", &mut r);
            r
        };
        assert!(run("width=,height=1").has_rule(Rule::ViewportSyntax));
        assert!(run("height=1600").has_rule(Rule::ViewportDimensionMissing));
        assert!(run("width=100px,height=200px").has_rule(Rule::ViewportValueInvalid));
        assert!(
            run("width=100,width=200,height=100").has_rule(Rule::ViewportPropertyRepeated),
            "a repeated dimension is HTM-059"
        );
        assert!(run("width=1200,height=1600").is_clean());
    }

    #[test]
    fn svg_rules_cover_use_fragments_and_the_fixed_layout_viewbox() {
        let mut r = Report::default();
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink">
            <use xlink:href="sprite.svg"/><use xlink:href="other.svg#sym"/><use href="#local"/>
            </svg>"##;
        check_content_schema(svg, "a.svg", false, true, &mut r);
        assert_eq!(
            r.violations
                .iter()
                .filter(|v| v.rule == Rule::SvgReferenceNoFragment)
                .count(),
            1,
            "only the fragment-less <use> fires, got:\n{r}"
        );
        // HTM-048 reads the *outermost* svg only.
        assert!(!outermost_svg_has_viewbox(
            r#"<svg xmlns="http://www.w3.org/2000/svg"/>"#
        ));
        assert!(outermost_svg_has_viewbox(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1 1"><svg/></svg>"#
        ));
        assert!(
            outermost_svg_has_viewbox("<svg"),
            "a malformed document is the well-formedness pass's finding, not this one's"
        );
    }

    #[test]
    fn empty_css_url_is_reported() {
        let (urls, empties) =
            css_url_tokens_with_empties("a{background:url()}b{src:url( )}c{d:url(x.png)}");
        assert_eq!(urls, ["x.png"]);
        assert_eq!(empties, 2);
    }

    #[test]
    fn opf_elements_that_require_children_must_have_them() {
        let run = |xml: &str| {
            let mut r = Report::default();
            check_opf_schema(xml, "content.opf", true, false, &mut r);
            r
        };
        let pkg = |body: &str| {
            format!(
                r#"<package xmlns="http://www.idpf.org/2007/opf" version="2.0"><metadata/>{body}</package>"#
            )
        };
        // `element guide { reference+ }`, `tours { tour+ }`, `tour { site+ }`.
        for empty in [
            "<guide></guide>",
            "<guide/>",
            "<tours></tours>",
            r#"<tours><tour title="t"></tour></tours>"#,
        ] {
            assert!(
                run(&pkg(empty)).has_rule(Rule::PackageIncomplete),
                "{empty} must fire RSC-005"
            );
        }
        let full = r#"<guide><reference type="cover" href="c.xhtml"/></guide><tours><tour title="t"><site title="s" href="a.xhtml"/></tour></tours>"#;
        assert!(
            run(&pkg(full)).is_clean(),
            "populated guide/tours are clean, got:\n{}",
            run(&pkg(full))
        );
        // A missing `<guide>` is not an incomplete one.
        assert!(run(&pkg("")).is_clean());
    }

    #[test]
    fn epub2_content_grammar_reads_the_xhtml11_attlists() {
        let run = |body: &str, epub2: bool| {
            let mut r = Report::default();
            let xml =
                format!(r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>{body}</body></html>"#);
            check_content_schema(&xml, "c.xhtml", epub2, !epub2, &mut r);
            r
        };
        // Attributes no XHTML 1.1 attlist declares, each seen in the corpus.
        for bad in [
            r#"<div xmlU0003Alang="en">x</div>"#,
            r#"<li value="3">x</li>"#,
            r#"<table><tr><td width="50%">x</td></tr></table>"#,
            r#"<p onclick="f()">x</p>"#,
            r#"<div data-x="1">x</div>"#,
        ] {
            assert!(
                run(bad, true).has_rule(Rule::DisallowedContentAttribute),
                "{bad} must fire RSC-005"
            );
            assert!(
                !run(bad, false).has_rule(Rule::DisallowedContentAttribute),
                "{bad} is EPUB 3's business, not this rule's"
            );
        }
        // The attlists these elements really do have.
        let ok = r#"<div class="c" style="s" xml:lang="en" title="t">
            <table summary="s" border="1"><tr align="left"><td style="p" colspan="2">x</td></tr></table>
            <pre xml:space="preserve">x</pre><a href="a" charset="utf-8">l</a>
            <blockquote cite="u">q</blockquote><img alt="" src="i.png" longdesc="d"/></div>"#;
        assert!(
            run(ok, true).is_clean(),
            "the declared attlists are legal, got:\n{}",
            run(ok, true)
        );
        // An element the schema declares nowhere — the forms, legacy and HTML5
        // vocabularies are all absent from `content.rng`.
        for absent in ["<u>x</u>", "<center>x</center>", "<section>x</section>"] {
            assert!(
                run(absent, true).has_rule(Rule::UndeclaredContentElement),
                "{absent} must fire RSC-005"
            );
            assert!(run(absent, false).is_clean(), "{absent} is legal in EPUB 3");
        }
    }

    #[test]
    fn epub3_content_rules_cover_meta_encoding_and_body_role() {
        let run = |body: &str| {
            let mut r = Report::default();
            check_content_schema(body, "c.xhtml", false, true, &mut r);
            r
        };
        let doc = |head: &str, body_attrs: &str| {
            format!(
                r#"<html xmlns="http://www.w3.org/1999/xhtml"><head>{head}</head><body{body_attrs}/></html>"#
            )
        };
        let meta =
            |content: &str| format!(r#"<meta http-equiv="Content-Type" content="{content}"/>"#);
        // `normalize-space` collapses only XML whitespace, so an ideographic
        // space between the parameters fails the assertion (the corpus case).
        for bad in ["text/html;\u{3000}charset=utf-8", "text/html", "text/plain"] {
            assert!(
                run(&doc(&meta(bad), "")).has_rule(Rule::MetaEncodingDeclaration),
                "content={bad:?} must fire RSC-005"
            );
        }
        for good in [
            "text/html; charset=utf-8",
            "text/html;charset=utf-8",
            "TEXT/HTML;  CHARSET=UTF-8",
            "  text/html;\n charset=utf-8 ",
        ] {
            assert!(
                run(&doc(&meta(good), "")).is_clean(),
                "content={good:?} is valid, got:\n{}",
                run(&doc(&meta(good), ""))
            );
        }
        // `<meta>` without http-equiv is untouched.
        assert!(run(&doc(r#"<meta charset="utf-8"/>"#, "")).is_clean());
        // `body.attrs` accepts four ARIA roles and no others.
        assert!(
            run(&doc("", r#" role="doc-cover""#)).has_rule(Rule::BodyRoleNotAllowed),
            "a DPUB role on <body> must fire"
        );
        for role in ["application", "document", "none", "presentation"] {
            assert!(
                run(&doc("", &format!(r#" role="{role}""#))).is_clean(),
                "role={role} is allowed on <body>"
            );
        }
    }

    #[test]
    fn empty_host_urls_are_invalid() {
        for bad in ["http://", "http:///a/b", "https://?q=1", "ftp://#f"] {
            assert!(url_has_empty_host(bad), "{bad:?} has no host");
            assert_eq!(invalid_url_reason(bad), Some("host is missing"));
        }
        for ok in [
            "http://x/",
            "file:///a/b", // a file: URL may legally have an empty host
            "../a.xhtml",
            "data:text/plain,x",
            "mailto:a@b",
            "kindle:embed:0007?mime=image/jpeg",
        ] {
            assert!(!url_has_empty_host(ok), "{ok:?} must not be flagged");
        }
        // The same emptiness fails XSD anyURI in the EPUB 2 content grammar.
        let mut r = Report::default();
        let xml = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><a href="http://">x</a></body></html>"#;
        check_content_schema(xml, "c.xhtml", true, false, &mut r);
        assert!(r.has_rule(Rule::UriAttributeInvalid), "got:\n{r}");
    }

    #[test]
    fn ncname_test_rejects_only_certainly_invalid_values() {
        // The corpus class (a leading digit), plus the colon the message names,
        // whitespace, and an empty value.
        for bad in [
            "1T142-50c5",
            "0-4358",
            "8ddbbe-65a9",
            "a:b",
            "a b",
            "a/b",
            "-lead",
            ".lead",
            "",
        ] {
            assert!(!may_be_ncname(bad), "{bad:?} is not an NCName");
        }
        // Valid NCNames, the deliberately-conservative non-ASCII cases (XML's
        // Letter production varies by edition, so they are never flagged), and
        // the whitespace-collapse cases the xsd:token base makes valid.
        for ok in [
            "a",
            "_x",
            "chapter-1",
            "p.1_2",
            "日本語",
            "é1",
            "_1",
            " x ",
            "nav ",
        ] {
            assert!(may_be_ncname(ok), "{ok:?} must not be flagged");
        }
    }

    #[test]
    fn content_schema_rules_are_version_gated() {
        let run = |xml: &str, epub2: bool, epub3: bool| {
            let mut r = Report::default();
            check_content_schema(xml, "c.xhtml", epub2, epub3, &mut r);
            r
        };
        // EPUB 2: XHTML 1.1 requires img/@alt, forbids `charset` on <meta>, types
        // `id` as xsd:ID (NCName), and its schematron rejects nested anchors.
        let e2 = r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><meta charset="UTF-8"/></head>
<body id="0-4358"><p><img src="i.jpg"/></p><a href="x"><a id="in">t</a></a></body></html>"#;
        let r = run(e2, true, false);
        assert!(
            r.has_rule(Rule::MissingRequiredAttribute),
            "img needs alt:\n{r}"
        );
        assert!(
            r.has_rule(Rule::DisallowedContentAttribute),
            "meta charset:\n{r}"
        );
        assert!(r.has_rule(Rule::IdNotNcName), "id must be an NCName:\n{r}");
        assert!(r.has_rule(Rule::NestedHyperlink), "nested anchors:\n{r}");

        // EPUB 3: every one of those is legal (HTML5 grammar, `id` is a token),
        // so the same document is clean.
        assert!(
            run(e2, false, true).is_clean(),
            "EPUB 3 content grammar allows all of it, got:\n{}",
            run(e2, false, true)
        );

        // Duplicate ids hold in both versions (id-unique.sch).
        let dup = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><p id="a">1</p><p id="a">2</p></body></html>"#;
        for (e2, e3) in [(true, false), (false, true)] {
            assert!(
                run(dup, e2, e3).has_rule(Rule::DuplicateId),
                "duplicate id must fire (epub2={e2})"
            );
        }

        // Elements outside the XHTML namespace are routed past the grammar, so a
        // foreign `<img>`/`<meta>` is never judged.
        let foreign = r#"<r xmlns="urn:x"><img src="i.jpg"/><meta charset="UTF-8"/></r>"#;
        assert!(run(foreign, true, false).is_clean(), "foreign namespace");

        // Attributes XHTML 1.1 declares nowhere — HTML5's `data-*`/`hidden`,
        // ARIA's `role`/`aria-*`, and EPUB 3's `epub:type` — plus `<html>`'s
        // short attlist (`style`/`class`/`id` are not in it).
        for body in [
            r#"<body data-cfi="/4">x</body>"#,
            r#"<body hidden="hidden">x</body>"#,
            r#"<p role="doc-chapter">x</p>"#,
            r#"<p aria-label="x">y</p>"#,
            r#"<body epub:type="toc">x</body>"#,
        ] {
            let xml = format!(
                r#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">{body}</html>"#
            );
            assert!(
                run(&xml, true, false).has_rule(Rule::DisallowedContentAttribute),
                "EPUB 2 must reject {body:?}"
            );
            assert!(
                run(&xml, false, true).is_clean(),
                "EPUB 3 allows {body:?}, got:\n{}",
                run(&xml, false, true)
            );
        }
        let styled_html = r#"<html xmlns="http://www.w3.org/1999/xhtml" xml:lang="en" style="font-size:1rem"><body>x</body></html>"#;
        assert!(
            run(styled_html, true, false).has_rule(Rule::DisallowedContentAttribute),
            "style is not in the EPUB 2 <html> attlist"
        );
        let plain_html = r#"<html xmlns="http://www.w3.org/1999/xhtml" xml:lang="en" lang="en" dir="ltr" version="-//W3C//DTD XHTML 1.1//EN"><body class="c" id="b">x</body></html>"#;
        assert!(
            run(plain_html, true, false).is_clean(),
            "the EPUB 2 <html> attlist (and ordinary body attributes) are legal, got:\n{}",
            run(plain_html, true, false)
        );
    }

    #[test]
    fn ncx_play_order_ports_the_schematron_assertions() {
        let nodes = |v: &[(&str, Option<&str>)]| -> Vec<(String, Option<String>)> {
            v.iter()
                .map(|(o, s)| (o.to_string(), s.map(str::to_string)))
                .collect()
        };
        let run = |v: Vec<(String, Option<String>)>| {
            let mut r = Report::default();
            check_ncx_play_order(&v, "toc.ncx", &mut r);
            r
        };
        // A well-formed sequence: 1,2,3 with distinct targets.
        assert!(
            run(nodes(&[
                ("1", Some("a.xhtml")),
                ("2", Some("b.xhtml")),
                ("3", Some("c.xhtml")),
            ]))
            .is_clean(),
            "a gapless sequence starting at 1 is clean"
        );
        // Same playOrder, different targets.
        assert!(
            run(nodes(&[("1", Some("a.xhtml")), ("1", Some("b.xhtml"))]))
                .has_rule(Rule::NcxPlayOrder),
            "identical playOrder with different targets must fire"
        );
        // Same playOrder, same target — legal (that is what the assertion allows).
        assert!(
            run(nodes(&[("1", Some("a.xhtml")), ("1", Some("a.xhtml"))])).is_clean(),
            "identical playOrder pointing at the same target is legal"
        );
        // A gap (3 has no 2) and a sequence that never starts at 1.
        assert!(
            run(nodes(&[("1", Some("a")), ("3", Some("c"))])).has_rule(Rule::NcxPlayOrder),
            "a gap must fire"
        );
        assert!(
            run(nodes(&[("2", Some("a")), ("3", Some("c"))])).has_rule(Rule::NcxPlayOrder),
            "a sequence not starting at 1 must fire"
        );
        // A node without a `<content src>` child takes part in no target
        // comparison (the XPath compares against an empty node-set).
        assert!(
            run(nodes(&[("1", None), ("1", Some("a"))])).is_clean(),
            "a targetless node never triggers the same-target assertion"
        );
    }

    #[test]
    fn dc_content_requirements_are_version_gated() {
        let run = |opf: &str, epub2: bool, epub3: bool| {
            let pkg = opf::parse(opf).unwrap();
            let mut r = Report::default();
            check_dc_content(&pkg, epub2, epub3, "content.opf", &mut r);
            r
        };
        // EPUB 3: every dc element must have content; EPUB 2 requires it only of
        // dc:identifier.
        let empty_source = r#"<package version="3.0"><metadata><dc:identifier id="u">urn:uuid:1</dc:identifier><dc:title>T</dc:title><dc:source/></metadata></package>"#;
        assert!(
            run(empty_source, false, true).has_rule(Rule::EmptyDcContent),
            "empty dc:source is an EPUB 3 schema violation"
        );
        assert!(
            run(empty_source, true, false).is_clean(),
            "EPUB 2 requires non-empty content only of dc:identifier"
        );
        let empty_id = r#"<package version="2.0"><metadata><dc:identifier id="isbn_"/><dc:title>T</dc:title></metadata></package>"#;
        assert!(
            run(empty_id, true, false).has_rule(Rule::EmptyDcContent),
            "empty dc:identifier fires in EPUB 2 too"
        );
        // At least one dc:identifier is required (EPUB 3 gate, like the sibling
        // title/language rules).
        let no_id =
            r#"<package version="3.0"><metadata><dc:title>T</dc:title></metadata></package>"#;
        assert!(
            run(no_id, false, true).has_rule(Rule::MissingIdentifier),
            "missing dc:identifier must fire"
        );
        assert!(run(no_id, true, false).is_clean(), "EPUB 2 gate");
    }

    #[test]
    fn spine_toc_must_be_declared_when_an_ncx_is_present() {
        let run = |opf: &str| {
            let pkg = opf::parse(opf).unwrap();
            let mut r = Report::default();
            check_package_schema_structure(&pkg, false, true, "content.opf", &mut r);
            r
        };
        let ncx_item =
            r#"<item id="toc.ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>"#;
        let missing = format!(
            r#"<package version="3.0"><manifest>{ncx_item}</manifest><spine><itemref idref="a"/></spine></package>"#
        );
        assert!(
            run(&missing).has_rule(Rule::SpineTocMissing),
            "an NCX in the manifest requires <spine toc>"
        );
        let declared = format!(
            r#"<package version="3.0"><manifest>{ncx_item}</manifest><spine toc="toc.ncx"><itemref idref="a"/></spine></package>"#
        );
        assert!(run(&declared).is_clean(), "declared toc is clean");
        // No NCX at all: the EPUB 3 rule has no context to fire in — but the
        // EPUB 2 grammar requires `toc` on `<spine>` unconditionally.
        let no_ncx = r#"<package version="3.0"><manifest><item id="a" href="a.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="a"/></spine></package>"#;
        assert!(run(no_ncx).is_clean(), "no NCX, no EPUB 3 requirement");
        let mut r = Report::default();
        check_package_schema_structure(
            &opf::parse(no_ncx).unwrap(),
            true,
            false,
            "content.opf",
            &mut r,
        );
        assert!(
            r.has_rule(Rule::SpineTocMissing),
            "the EPUB 2 spine attlist makes `toc` mandatory"
        );

        // An absent (or empty) manifest/spine is incomplete in both grammars.
        let no_spine = r#"<package version="3.0"><manifest><item id="a" href="a.xhtml" media-type="application/xhtml+xml"/></manifest></package>"#;
        assert!(
            run(no_spine).has_rule(Rule::PackageIncomplete),
            "a package without a spine is incomplete"
        );
        let no_manifest =
            r#"<package version="3.0"><spine toc="x"><itemref idref="a"/></spine></package>"#;
        assert!(
            run(no_manifest).has_rule(Rule::PackageIncomplete),
            "a package without manifest items is incomplete"
        );
    }

    #[test]
    fn well_formedness_namespace_and_entity_edges() {
        // A prefix declared on the element or an ancestor is bound; `xml` is
        // always bound without a declaration.
        for xml in [
            r#"<package xmlns="urn:x" xmlns:opf="urn:opf"><id opf:scheme="ASIN">x</id></package>"#,
            r#"<r xmlns:opf="urn:opf"><child><id opf:scheme="ASIN">x</id></child></r>"#,
            r#"<r xml:lang="en" xml:space="preserve">x</r>"#,
        ] {
            let mut r = Report::default();
            check_xml_well_formedness(xml, "x.xml", &mut r);
            assert!(
                r.is_clean(),
                "bound prefixes should pass: {xml:?}, got:\n{r}"
            );
        }
        // The general-entity class is out of scope (epubcheck resolves named
        // entities via the DTD its DOCTYPE catalogues, so a plain predefined-only
        // check would false-positive on `&nbsp;` under an XHTML DOCTYPE). These
        // must NOT be flagged, whether predefined, numeric, or named.
        for xml in [
            "<r>&amp; &lt; &gt; &#160; &#x20;</r>",
            "<r>&nbsp;&atilde;&dagger;</r>",
        ] {
            let mut r = Report::default();
            check_xml_well_formedness(xml, "x.xml", &mut r);
            assert!(
                r.is_clean(),
                "entity references are out of scope (no false positive): {xml:?}, got:\n{r}"
            );
        }
    }

    #[test]
    fn unbound_opf_prefix_in_package_document_flags_rsc016_end_to_end() {
        // The real corpus class (books 2087/2088): <dc:identifier opf:scheme=…>
        // with no xmlns:opf on the package. RSC-016 must fire via the OPF path.
        let opf = r#"<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="u" version="3.0">
          <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
            <dc:title>t</dc:title>
            <dc:language>en</dc:language>
            <dc:identifier id="u" opf:scheme="ASIN">X</dc:identifier>
          </metadata>
          <manifest><item id="c" href="c.xhtml" media-type="application/xhtml+xml" properties="nav"/></manifest>
          <spine><itemref idref="c"/></spine>
        </package>"#;
        let doc = "<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\"><head><title>t</title></head><body><nav epub:type=\"toc\"><ol><li><a href=\"c.xhtml\">c</a></li></ol></nav></body></html>";
        let epub = zip_epub(opf, &[("c.xhtml", doc)]);
        let report = validate(&epub);
        assert!(
            report.has_rule(Rule::MalformedXml),
            "unbound opf: prefix in the OPF should flag RSC-016, got:\n{report}"
        );
    }

    #[test]
    fn css_url_extraction_handles_quotes_imports_comments_and_skips() {
        let css = r#"
            /* url(commented-out.png) must be ignored */
            @import "base.css";
            @import url(theme.css);
            body { background: URL('img/bg.png'); }
            @font-face { src: url("../fonts/x.woff2") format("woff2"), url(fallback.ttf); }
            .a { background: url( data:image/png;base64,AAAA ); }
            .b { background: url(https://cdn.example.com/x.png); }
            .c { filter: url(#local-frag); }
        "#;
        let urls = extract_css_urls(css);
        for want in [
            "base.css",
            "theme.css",
            "img/bg.png",
            "../fonts/x.woff2",
            "fallback.ttf",
        ] {
            assert!(
                urls.iter().any(|u| u == want),
                "missing {want:?} in {urls:?}"
            );
        }
        // Commented-out, data:, remote, and pure-fragment targets are excluded.
        assert!(!urls.iter().any(|u| u.contains("commented-out")));
        assert!(!urls.iter().any(|u| u.starts_with("data:")));
        assert!(!urls.iter().any(|u| u.contains("cdn.example.com")));
        assert!(!urls.iter().any(|u| u.starts_with('#')));
    }

    #[test]
    fn css_references_flag_missing_and_leaking_targets() {
        let opf = r##"<package version="3.0" unique-identifier="u">
          <metadata><dc:title>t</dc:title><dc:language>en</dc:language>
            <dc:identifier id="u">urn:uuid:f47ac10b-58cc-4372-a567-0e02b2c3d479</dc:identifier>
            <meta property="dcterms:modified">2020-01-01T00:00:00Z</meta></metadata>
          <manifest>
            <item id="c" href="c.xhtml" media-type="application/xhtml+xml"/>
            <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
            <item id="css" href="style.css" media-type="text/css"/>
            <item id="font" href="fonts/present.woff2" media-type="font/woff2"/>
          </manifest>
          <spine><itemref idref="c"/><itemref idref="nav"/></spine>
        </package>"##;
        // One url() resolves to a present font (clean); one to a missing file
        // (RSC-007); one escapes the container from OEBPS depth (RSC-026).
        let css = r#"@font-face { src: url("fonts/present.woff2"); }
                     @font-face { src: url("fonts/missing.woff2"); }
                     @font-face { src: url("../../escape.ttf"); }"#;
        let doc = |t: &str| {
            format!(
                r#"<?xml version="1.0" encoding="utf-8"?><!DOCTYPE html>
              <html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
              <head><title>{t}</title></head><body>{t}</body></html>"#
            )
        };
        let nav = r#"<?xml version="1.0" encoding="utf-8"?><!DOCTYPE html>
          <html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
          <head><title>nav</title></head>
          <body><nav epub:type="toc"><ol><li><a href="c.xhtml">c</a></li></ol></nav></body></html>"#;
        let bytes = zip_epub(
            opf,
            &[
                ("c.xhtml", &doc("c")),
                ("nav.xhtml", nav),
                ("style.css", css),
                ("fonts/present.woff2", "woff-bytes"),
            ],
        );
        let report = validate(&bytes);
        let broken = report
            .violations
            .iter()
            .filter(|v| v.rule == Rule::BrokenHref && v.message.contains("missing.woff2"))
            .count();
        let leak = report
            .violations
            .iter()
            .filter(|v| v.rule == Rule::HrefEscapesOpfRoot && v.message.contains("escape.ttf"))
            .count();
        assert_eq!(broken, 1, "RSC-007 for the missing font; got:\n{report}");
        assert_eq!(leak, 1, "RSC-026 for the escaping font; got:\n{report}");
        // The present font must not be flagged.
        assert!(
            !report
                .violations
                .iter()
                .any(|v| v.message.contains("present.woff2")),
            "present font wrongly flagged:\n{report}"
        );
    }

    #[test]
    fn css_opaque_scheme_reference_flags_rsc008() {
        // `css_url_tokens` surfaces every raw target; the relative-only extractor
        // drops scheme'd ones, so they route through the RSC-008 pass instead.
        let src = "@font-face{src:url(kindle:embed:0001);} .x{background:url(bg.png)}";
        let toks = css_url_tokens(src);
        assert!(toks.iter().any(|t| t == "kindle:embed:0001"));
        assert!(toks.iter().any(|t| t == "bg.png"));
        let rel = extract_css_urls(src);
        assert!(rel.iter().any(|t| t == "bg.png"));
        assert!(!rel.iter().any(|t| t.starts_with("kindle:")));

        // End-to-end: an undeclared `kindle:embed:…` CSS reference is RSC-008.
        let opf = r##"<package version="3.0" unique-identifier="u">
          <metadata><dc:title>t</dc:title><dc:language>en</dc:language>
            <dc:identifier id="u">urn:uuid:f47ac10b-58cc-4372-a567-0e02b2c3d479</dc:identifier>
            <meta property="dcterms:modified">2020-01-01T00:00:00Z</meta></metadata>
          <manifest>
            <item id="c" href="c.xhtml" media-type="application/xhtml+xml"/>
            <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
            <item id="css" href="style.css" media-type="text/css"/>
          </manifest>
          <spine><itemref idref="c"/><itemref idref="nav"/></spine>
        </package>"##;
        let doc = r#"<?xml version="1.0" encoding="utf-8"?><!DOCTYPE html>
          <html xmlns="http://www.w3.org/1999/xhtml"><head><title>c</title></head><body>c</body></html>"#;
        let nav = r#"<?xml version="1.0" encoding="utf-8"?><!DOCTYPE html>
          <html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
          <head><title>nav</title></head><body><nav epub:type="toc"><ol><li><a href="c.xhtml">c</a></li></ol></nav></body></html>"#;
        // In `@font-face` (a FONT reference, remote-exempt in EPUB 3) → RSC-008.
        let font_css = "@font-face { src: url(kindle:embed:0001); }";
        let bytes = zip_epub(
            opf,
            &[
                ("c.xhtml", doc),
                ("nav.xhtml", nav),
                ("style.css", font_css),
            ],
        );
        let report = validate(&bytes);
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.rule == Rule::ReferenceNotInManifest
                    && v.message.contains("kindle:embed:0001")),
            "kindle:embed @font-face reference should be RSC-008, got:\n{report}"
        );
        // Outside `@font-face` (an image reference) the same remote target is not
        // allowed → RSC-006, and must NOT be RSC-008.
        let img_css = ".x { background: url(kindle:embed:000A?mime=image/jpeg); }";
        let bytes = zip_epub(
            opf,
            &[("c.xhtml", doc), ("nav.xhtml", nav), ("style.css", img_css)],
        );
        let report = validate(&bytes);
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.rule == Rule::RemoteResourceNotAllowed),
            "kindle:embed image reference should be RSC-006, got:\n{report}"
        );
        assert!(
            !report
                .violations
                .iter()
                .any(|v| v.rule == Rule::ReferenceNotInManifest),
            "an image-context remote reference must not be RSC-008, got:\n{report}"
        );
    }

    #[test]
    fn corrupt_image_detected_but_valid_image_passes() {
        // A genuine 2x2 PNG decodes to its dimensions → not corrupt.
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(2, 2))
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert!(!image_fails_to_decode(&png));

        // Garbage bytes with no recognizable image signature → corrupt (the
        // real-corpus case: a `.png` whose content is not a PNG at all).
        assert!(image_fails_to_decode(&[
            0xB5, 0xDF, 0xCB, 0xA1, 0x2F, 0x36, 0xB9, 0x00, 0x90, 0x07, 0x5D, 0x7C
        ]));

        // A valid PNG signature but a truncated IHDR → dimensions unreadable →
        // corrupt (a truncated file, epubcheck's other PKG-021 path).
        assert!(image_fails_to_decode(&[
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00
        ]));
    }

    /// Assemble a minimal EPUB zip: `mimetype` (stored), `container.xml`, an OPF
    /// at `OEBPS/content.opf`, and the given `OEBPS/<name>` documents. For the
    /// content-conformance / property checks that read content documents.
    fn zip_epub(opf: &str, docs: &[(&str, &str)]) -> Vec<u8> {
        use zip::write::{SimpleFileOptions, ZipWriter};
        let mut out = Vec::new();
        {
            let mut z = ZipWriter::new(Cursor::new(&mut out));
            let stored =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            z.start_file("mimetype", stored).unwrap();
            std::io::Write::write_all(&mut z, b"application/epub+zip").unwrap();
            let deflated = SimpleFileOptions::default();
            z.start_file("META-INF/container.xml", deflated).unwrap();
            std::io::Write::write_all(
                &mut z,
                br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            )
            .unwrap();
            z.start_file("OEBPS/content.opf", deflated).unwrap();
            std::io::Write::write_all(&mut z, opf.as_bytes()).unwrap();
            for (name, body) in docs {
                z.start_file(format!("OEBPS/{name}"), deflated).unwrap();
                std::io::Write::write_all(&mut z, body.as_bytes()).unwrap();
            }
            z.finish().unwrap();
        }
        out
    }

    #[test]
    fn url_scheme_classification() {
        assert_eq!(
            url_scheme("https://example.com/a").as_deref(),
            Some("https")
        );
        assert_eq!(
            url_scheme("DATA:image/png;base64,AA").as_deref(),
            Some("data")
        );
        assert_eq!(url_scheme("file:///etc/x").as_deref(), Some("file"));
        // Relative references have no scheme.
        assert_eq!(url_scheme("text/ch1.xhtml"), None);
        assert_eq!(url_scheme("../images/p.jpg"), None);
        assert_eq!(url_scheme("#frag"), None);
        assert_eq!(url_scheme("a/b:c.xhtml"), None); // ':' after a path '/'

        assert!(is_data_url("data:text/plain,hi"));
        assert!(!is_data_url("https://x/y"));
        assert!(is_remote_href("https://x/y"));
        assert!(is_remote_href("http://x/y"));
        assert!(!is_remote_href("data:text/plain,hi")); // inline, not remote
        assert!(!is_remote_href("file:///x")); // handled by RSC-030
        assert!(!is_remote_href("text/ch1.xhtml")); // local
    }

    #[test]
    fn remote_and_data_url_manifest_checks() {
        // local spine item (clean); data-URL item (RSC-029); remote spine item
        // (RSC-006); remote audio NOT in spine (exempt → clean); remote image NOT
        // in spine (the deferred non-spine case → clean under the spine-only port).
        let opf = r##"<package version="3.0" unique-identifier="uid">
          <metadata><dc:identifier id="uid">x</dc:identifier></metadata>
          <manifest>
            <item id="local" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
            <item id="dataimg" href="data:image/png;base64,AAAA" media-type="image/png"/>
            <item id="remotespine" href="https://ex.com/ch2.xhtml" media-type="application/xhtml+xml"/>
            <item id="remoteaudio" href="https://ex.com/a.mp3" media-type="audio/mpeg"/>
            <item id="remoteimg" href="https://ex.com/p.png" media-type="image/png"/>
          </manifest>
          <spine>
            <itemref idref="local"/>
            <itemref idref="remotespine"/>
          </spine>
        </package>"##;
        let pkg = opf::parse(opf).unwrap();
        let mut r = Report::default();
        check_remote_and_data_urls(&pkg, "content.opf", &mut r);
        assert!(r.has_rule(Rule::DataUrlNotAllowed), "data: URL → RSC-029");
        assert!(
            r.has_rule(Rule::RemoteResourceInSpine),
            "remote spine item → RSC-006"
        );
        // Exactly those two: remote audio (exempt) and remote non-spine image
        // (deferred) must NOT fire.
        assert_eq!(r.violations.len(), 2, "unexpected extra findings:\n{r}");
    }

    #[test]
    fn opf_link_cluster_checks() {
        let opf = r##"<package version="3.0" unique-identifier="uid">
          <metadata>
            <dc:identifier id="uid">x</dc:identifier>
            <link rel="alternate mapping" href="m.xml" media-type="application/xml"/>
            <link rel="voicing" href="say.txt" media-type="text/plain"/>
            <link rel="record" href="#meta"/>
            <link href="data:text/plain,hi"/>
            <link rel="record" href="extra.xhtml"/>
          </metadata>
          <manifest>
            <item id="c" href="ch.xhtml" media-type="application/xhtml+xml"/>
            <item id="extra" href="extra.xhtml" media-type="application/xhtml+xml"/>
            <item id="map" href="m.xml" media-type="application/xml"/>
          </manifest>
          <spine><itemref idref="c"/></spine>
        </package>"##;
        let pkg = opf::parse(opf).unwrap();
        let mut r = Report::default();
        check_opf_links(&pkg, "", "content.opf", &mut r);
        assert!(
            r.has_rule(Rule::LinkAlternatePaired),
            "alternate+mapping → OPF-089"
        );
        assert!(
            r.has_rule(Rule::VoicingLinkNotAudio),
            "voicing text/plain → OPF-095"
        );
        assert!(
            r.has_rule(Rule::LinkIntoPackageDocument),
            "href=#meta → OPF-098"
        );
        assert!(r.has_rule(Rule::DataUrlNotAllowed), "data: link → RSC-029");
        assert!(
            r.has_rule(Rule::LinkToNonSpineManifestItem),
            "link to non-spine extra.xhtml → OPF-067"
        );
    }

    #[test]
    fn opf_links_clean_for_a_well_formed_package() {
        // A voicing link with an audio type, and a metadata record link to a
        // resource that is *not* a manifest item (so no OPF-067): all clean.
        let opf = r##"<package version="3.0" unique-identifier="uid">
          <metadata>
            <dc:identifier id="uid">x</dc:identifier>
            <link rel="voicing" href="v.mp3" media-type="audio/mpeg"/>
            <link rel="record" href="onix.xml" media-type="application/xml"/>
          </metadata>
          <manifest><item id="c" href="ch.xhtml" media-type="application/xhtml+xml"/></manifest>
          <spine><itemref idref="c"/></spine>
        </package>"##;
        let pkg = opf::parse(opf).unwrap();
        let mut r = Report::default();
        check_opf_links(&pkg, "", "content.opf", &mut r);
        assert!(r.is_clean(), "well-formed links should be clean, got:\n{r}");
    }

    #[test]
    fn hyperlink_target_rule_matches_epubcheck() {
        let mk = |id: &str, mt: &str, fb: Option<&str>| opf::ManifestItem {
            id: id.to_string(),
            href: format!("{id}.x"),
            media_type: mt.to_string(),
            properties: vec![],
            fallback: fb.map(str::to_string),
            fallback_style: None,
        };
        let xhtml_spine = mk("c", "application/xhtml+xml", None);
        let xhtml_aside = mk("aside", "application/xhtml+xml", None);
        let svg_spine = mk("s", "image/svg+xml", None);
        let image = mk("img", "image/jpeg", None);
        let image_fb = mk("imgfb", "image/jpeg", Some("c")); // fallback → content doc
        let items = [&xhtml_spine, &xhtml_aside, &svg_spine, &image, &image_fb];
        let by_id: HashMap<&str, &opf::ManifestItem> =
            items.iter().map(|m| (m.id.as_str(), *m)).collect();
        let spine: HashSet<&str> = ["c", "s"].into_iter().collect();

        // Content doc in spine → clean (XHTML and SVG).
        assert_eq!(
            hyperlink_target_rule(&xhtml_spine, &by_id, &spine, false),
            None
        );
        assert_eq!(
            hyperlink_target_rule(&svg_spine, &by_id, &spine, false),
            None
        );
        // Content doc not in spine → RSC-011.
        assert_eq!(
            hyperlink_target_rule(&xhtml_aside, &by_id, &spine, false),
            Some(Rule::HyperlinkToNonSpineItem)
        );
        // Non-content type, no fallback → RSC-010.
        assert_eq!(
            hyperlink_target_rule(&image, &by_id, &spine, false),
            Some(Rule::HyperlinkToNonContentDocument)
        );
        // Non-content type but with a content-doc fallback, not in spine → RSC-011.
        assert_eq!(
            hyperlink_target_rule(&image_fb, &by_id, &spine, false),
            Some(Rule::HyperlinkToNonSpineItem)
        );
    }

    #[test]
    fn nav_remote_links_flags_only_remote_in_governed_navs() {
        let nav = r##"<html xmlns:epub="http://www.idpf.org/2007/ops">
          <body>
            <nav epub:type="toc">
              <ol>
                <li><a href="ch1.xhtml">Local</a></li>
                <li><a href="https://ex.com/ch2.xhtml">Remote</a></li>
              </ol>
            </nav>
            <nav epub:type="landmarks">
              <ol><li><a href="http://ex.com/cover.xhtml">Remote cover</a></li></ol>
            </nav>
            <nav epub:type="foo">
              <ol><li><a href="https://ex.com/other.xhtml">Remote, ungoverned nav</a></li></ol>
            </nav>
          </body>
        </html>"##;
        let hits = nav_remote_links(nav);
        // The toc and landmarks remote links fire; the local one and the
        // ungoverned "foo" nav do not.
        assert_eq!(hits.len(), 2, "got {hits:?}");
        assert!(hits.iter().any(|(k, h)| k == "toc" && h.contains("ch2")));
        assert!(
            hits.iter()
                .any(|(k, h)| k == "landmarks" && h.contains("cover"))
        );
    }

    #[test]
    fn idpf_obfuscated_uris_selects_only_the_idpf_algorithm() {
        let enc = r#"<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container"
            xmlns:enc="http://www.w3.org/2001/04/xmlenc#">
          <enc:EncryptedData>
            <enc:EncryptionMethod Algorithm="http://www.idpf.org/2008/embedding"/>
            <enc:CipherData><enc:CipherReference URI="OEBPS/fonts/obf.otf"/></enc:CipherData>
          </enc:EncryptedData>
          <enc:EncryptedData>
            <enc:EncryptionMethod Algorithm="http://ns.adobe.com/pdf/enc#RC"/>
            <enc:CipherData><enc:CipherReference URI="OEBPS/fonts/adobe.otf"/></enc:CipherData>
          </enc:EncryptedData>
          <enc:EncryptedData>
            <enc:EncryptionMethod Algorithm="http://www.w3.org/2001/04/xmlenc#aes256-cbc"/>
            <enc:CipherData><enc:CipherReference URI="OEBPS/secret.bin"/></enc:CipherData>
          </enc:EncryptedData>
        </encryption>"#;
        // Only the IDPF-obfuscated resource is returned (Adobe/real-encryption skipped).
        assert_eq!(idpf_obfuscated_uris(enc), vec!["OEBPS/fonts/obf.otf"]);
    }

    #[test]
    fn obfuscated_fonts_flag_non_font_only() {
        let enc = r#"<encryption xmlns:enc="http://www.w3.org/2001/04/xmlenc#">
          <enc:EncryptedData><enc:EncryptionMethod Algorithm="http://www.idpf.org/2008/embedding"/>
            <enc:CipherData><enc:CipherReference URI="OEBPS/f.otf"/></enc:CipherData></enc:EncryptedData>
          <enc:EncryptedData><enc:EncryptionMethod Algorithm="http://www.idpf.org/2008/embedding"/>
            <enc:CipherData><enc:CipherReference URI="OEBPS/pic.png"/></enc:CipherData></enc:EncryptedData>
        </encryption>"#;
        let opf = r#"<package version="3.0" unique-identifier="u">
          <metadata><dc:identifier id="u">x</dc:identifier></metadata>
          <manifest>
            <item id="f" href="f.otf" media-type="font/otf"/>
            <item id="p" href="pic.png" media-type="image/png"/>
          </manifest><spine/></package>"#;
        let pkg = opf::parse(opf).unwrap();
        // Mirror check_obfuscated_fonts: the font is exempt, the image is PKG-026.
        let flagged: Vec<&str> = idpf_obfuscated_uris(enc)
            .iter()
            .filter_map(|u| {
                pkg.manifest
                    .iter()
                    .find(|m| join_opf("OEBPS", &m.href) == *u)
            })
            .filter(|m| !is_blessed_font_type(&m.media_type))
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(flagged, vec!["p"], "only the obfuscated image is PKG-026");
    }

    #[test]
    fn image_header_magic_matches_epubcheck() {
        // Correct signatures → no mismatch.
        assert!(!image_header_mismatches(
            "image/jpeg",
            &[0xFF, 0xD8, 0xFF, 0xE0]
        ));
        assert!(!image_header_mismatches(
            "image/png",
            &[0x89, b'P', b'N', b'G', 0x0D]
        ));
        assert!(!image_header_mismatches("image/gif", b"GIF89a"));
        // Wrong signatures → mismatch (e.g. a PNG mislabeled as JPEG).
        assert!(image_header_mismatches(
            "image/jpeg",
            &[0x89, b'P', b'N', b'G']
        ));
        assert!(image_header_mismatches("image/png", b"GIF89a"));
        assert!(image_header_mismatches(
            "image/gif",
            &[0xFF, 0xD8, 0xFF, 0xE0]
        ));
        // Unchecked types are never a mismatch, whatever the bytes.
        assert!(!image_header_mismatches("image/svg+xml", b"<svg"));
        assert!(!image_header_mismatches("image/webp", &[0, 1, 2, 3]));
        // Too few bytes to judge → not flagged (corruption is a separate concern).
        assert!(!image_header_mismatches("image/png", &[0x89]));
    }

    #[test]
    fn percent_decoding_and_path_resolution() {
        assert_eq!(percent_decode("a%20b.xhtml"), "a b.xhtml");
        assert_eq!(percent_decode("no-escapes"), "no-escapes");
        assert_eq!(percent_decode("%E2%9C%93"), "✓"); // UTF-8 multi-byte
        assert_eq!(percent_decode("bad%2"), "bad%2"); // truncated → literal
        assert_eq!(percent_decode("%zz"), "%zz"); // non-hex → literal
        // A percent-encoded reference resolves to the literal (decoded) zip name.
        assert_eq!(
            resolve_href("OEBPS/text/ch.xhtml", "../images/a%20b.png").as_deref(),
            Some("OEBPS/images/a b.png")
        );
        assert_eq!(join_opf("OEBPS", "a%20b.xhtml"), "OEBPS/a b.xhtml");
    }

    /// Everything below validates a real EPUB, and the only EPUB *writer* in
    /// the crate is the Aozora builder — so these tests exist only in a build
    /// that has it.
    #[cfg(feature = "aozora")]
    mod on_a_generated_epub {
        use super::super::*;
        use crate::formats::aozora::{Document, EpubInput, TocEntry, build_epub};

        fn tiny_jpeg() -> Vec<u8> {
            // A genuine, decodable 2x2 JPEG — like a real cover. (A bare JFIF
            // header with no Start-of-Frame is not decodable; epubcheck and bokai
            // both flag that as a corrupt image, PKG-021.)
            let mut out = Vec::new();
            image::DynamicImage::ImageRgb8(image::RgbImage::new(2, 2))
                .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Jpeg)
                .unwrap();
            out
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
        fn non_linear_item_reachable_via_ncx_is_not_flagged() {
            // OPF-096 must NOT fire when the only inbound reference to a
            // linear="no" item is an NCX `<content src>`: epubcheck's NCXHandler
            // registers those as HYPERLINK references, so the item is reachable.
            // (Paired with `detects_non_linear_cover_without_hyperlink`, where
            // nothing — NCX included — points at the non-linear cover.) This is
            // the fix for a systematic false positive: real books whose only path
            // to a non-linear cover is the NCX toc.
            let bytes = sample_aozora_epub();
            // Cover becomes non-linear...
            let m1 = rewrite_zip_entry(&bytes, "OEBPS/content.opf", |opf| {
                opf.replace(
                    r#"<itemref idref="cover"/>"#,
                    r#"<itemref idref="cover" linear="no"/>"#,
                )
            });
            // ...but reachable: point an NCX navPoint at the cover.
            let m2 = rewrite_zip_entry(&m1, "OEBPS/toc.ncx", |ncx| {
                ncx.replace(
                    r#"<content src="text/title.xhtml"/>"#,
                    r#"<content src="text/cover.xhtml"/>"#,
                )
            });
            let report = validate(&m2);
            assert!(
                !report.has_rule(Rule::NonLinearUnreachable),
                "an NCX-referenced non-linear item must not trip OPF-096, got:\n{report}"
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
        fn scheme_url_with_query_is_not_rsc033() {
            // A scheme'd URL (`kindle:embed:…?mime=…`, common in Kindle-origin
            // EPUBs) is not a relative reference — its '?' is part of an opaque
            // path, not a query — so it must not trip RSC-033. epubcheck resolves
            // it and reports RSC-007 (broken href) instead; the paired
            // `detects_relative_url_with_query` proves a real relative '?' still
            // fires.
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "OEBPS/text/title.xhtml", |x| {
                x.replace(
                    "</body>",
                    r#"<img src="kindle:embed:0007?mime=image/jpg"/></body>"#,
                )
            });
            let report = validate(&mutated);
            assert!(
                !report.has_rule(Rule::RelativeUrlWithQuery),
                "a scheme URL must not trip RSC-033, got:\n{report}"
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
        fn detects_dangling_fragment_in_ncx() {
            // RSC-012 must also fire on an NCX `<content src="…#frag">` whose
            // fragment names no id in the target — epubcheck resolves NCX
            // navigation targets exactly as it does nav.xhtml hrefs. The clean
            // sample's NCX carries a *resolvable* `#h1` (the chapter navPoint),
            // so `aozora_output_passes_after_fix` is the paired proof this does
            // not false-fire on a valid NCX fragment.
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "OEBPS/toc.ncx", |ncx| {
                ncx.replace(
                    r#"<content src="text/title.xhtml"/>"#,
                    r#"<content src="text/title.xhtml#nope-frag"/>"#,
                )
            });
            let report = validate(&mutated);
            assert!(
                report.has_rule(Rule::FragmentNotDefined),
                "expected RSC-012 from the NCX, got:\n{report}"
            );
        }

        #[test]
        fn detects_missing_ncx_target() {
            // RSC-007: an NCX `<content src>` pointing at a file absent from the
            // container is a broken navigation target (the clean sample's NCX
            // points at a real file, so this does not false-fire otherwise).
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "OEBPS/toc.ncx", |ncx| {
                ncx.replace(
                    r#"<content src="text/title.xhtml"/>"#,
                    r#"<content src="text/nonexistent.xhtml"/>"#,
                )
            });
            let report = validate(&mutated);
            assert!(
                report
                    .violations
                    .iter()
                    .any(|v| v.rule == Rule::BrokenHref && v.message.contains("NCX")),
                "expected an NCX RSC-007, got:\n{report}"
            );
        }

        #[test]
        fn detects_ncx_uid_mismatch() {
            // NCX-001: change only the NCX `dtb:uid` so it diverges from the OPF
            // unique identifier. (The untouched epub validating clean is the
            // paired proof a matching uid does not fire.)
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "OEBPS/toc.ncx", |ncx| {
                ncx.replace("urn:uuid:", "urn:mismatch:")
            });
            let report = validate(&mutated);
            assert!(
                report.has_rule(Rule::NcxUidMismatch),
                "expected NCX-001, got:\n{report}"
            );
        }

        #[test]
        fn detects_reference_to_undeclared_container_file() {
            // RSC-008: a reference resolving to a file present in the container
            // but absent from the manifest (`mimetype`) must be caught.
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "OEBPS/text/title.xhtml", |x| {
                x.replace("</body>", r#"<a href="../../mimetype">x</a></body>"#)
            });
            let report = validate(&mutated);
            assert!(
                report.has_rule(Rule::ReferenceNotInManifest),
                "expected RSC-008, got:\n{report}"
            );
        }

        #[test]
        fn detects_rootfile_missing_full_path() {
            // OPF-016: a <rootfile> with no full-path attribute.
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "META-INF/container.xml", |c| {
                c.replace(r#"full-path="OEBPS/content.opf" "#, "")
            });
            let report = validate(&mutated);
            assert!(
                report.has_rule(Rule::RootfileMissingFullPath),
                "expected OPF-016, got:\n{report}"
            );
        }

        #[test]
        fn detects_rootfile_empty_full_path() {
            // OPF-017: a <rootfile> whose full-path is empty.
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "META-INF/container.xml", |c| {
                c.replace(r#"full-path="OEBPS/content.opf""#, r#"full-path="""#)
            });
            let report = validate(&mutated);
            assert!(
                report.has_rule(Rule::RootfileEmptyFullPath),
                "expected OPF-017, got:\n{report}"
            );
        }

        #[test]
        fn detects_non_utf8_xml_declaration() {
            // RSC-028: a content document declaring a non-UTF-8 charset. The
            // bytes stay UTF-8 (only the declaration changes), so the sniffer —
            // not the decoder — is what must catch it.
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "OEBPS/text/title.xhtml", |x| {
                x.replace(r#"encoding="UTF-8""#, r#"encoding="Shift_JIS""#)
            });
            let report = validate(&mutated);
            assert!(
                report.has_rule(Rule::XmlEncodingNotUtf8),
                "expected RSC-028, got:\n{report}"
            );
        }

        #[test]
        fn detects_opf_doctype_external_identifier() {
            // OPF-073: an external identifier in the package document's DOCTYPE.
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "OEBPS/content.opf", |opf| {
                opf.replace(
                    "<package",
                    "<!DOCTYPE package PUBLIC \"-//X//DTD//EN\" \"x.dtd\">\n<package",
                )
            });
            let report = validate(&mutated);
            assert!(
                report.has_rule(Rule::DoctypeExternalIdentifier),
                "expected OPF-073, got:\n{report}"
            );
        }

        #[test]
        fn detects_href_escaping_opf_root() {
            // RSC-026 is a *container-root* escape, not an OPF-dir escape. From
            // `OEBPS/text/title.xhtml`, `../../../escape.xhtml` rises above the zip
            // root (three `..` from a depth-2 directory) — must flag. A reference
            // resolving to a sibling in-container path (e.g. `../../escape.xhtml`
            // → `escape.xhtml` at the zip root) is legal and never RSC-026.
            //
            // The leak does not end the reference's life: the WHATWG parser clamps
            // the surplus `..` at the container root, so the reference is still
            // existence-checked and a missing target is *also* RSC-007. Verified
            // against epubcheck 5.3.0 on this exact document — it reports both.
            let bytes = sample_aozora_epub();
            let mutated = rewrite_zip_entry(&bytes, "OEBPS/text/title.xhtml", |xhtml| {
                xhtml.replace(
                    "</body>",
                    r#"<a href="../../../escape.xhtml">link</a></body>"#,
                )
            });
            let report = validate(&mutated);
            assert!(
                report.has_rule(Rule::HrefEscapesOpfRoot),
                "expected HrefEscapesOpfRoot, got:\n{}",
                report
            );
            assert!(
                report.has_rule(Rule::BrokenHref),
                "the clamped target is missing, so RSC-007 fires too:\n{}",
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

        #[test]
        fn added_errors_reports_only_what_an_edit_introduced() {
            use crate::validate::source::added_errors;
            let clean = sample_aozora_epub();
            // A wild book that is already invalid: two manifest items point at
            // files the container does not hold.
            let before = rewrite_zip_entry(&clean, "OEBPS/content.opf", |opf| {
                opf.replace(
                    r#"<item id="style" href="style.css" media-type="text/css"/>"#,
                    r#"<item id="style" href="nope.css" media-type="text/css"/><item id="x" href="gone.css" media-type="text/css"/>"#,
                )
            });
            assert!(
                added_errors(&before, &before).is_empty(),
                "a no-op edit adds nothing, however invalid the book already is"
            );
            // An edit that introduces one *more* broken reference is reported
            // once — the pre-existing ones are not re-reported.
            let after = rewrite_zip_entry(&before, "OEBPS/content.opf", |opf| {
                opf.replace(
                    r#"<item id="x" href="gone.css" media-type="text/css"/>"#,
                    r#"<item id="x" href="gone.css" media-type="text/css"/><item id="y" href="also-gone.css" media-type="text/css"/>"#,
                )
            });
            let added = added_errors(&before, &after);
            assert_eq!(added.len(), 1, "expected exactly one new error: {added:?}");
            assert_eq!(added[0].rule, "RSC-001");
            assert!(added[0].message.contains("also-gone.css"));
            // A repair removes findings and adds none.
            assert!(
                added_errors(&before, &clean).is_empty(),
                "fixing a book must not read as adding defects"
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
