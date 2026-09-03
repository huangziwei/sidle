//! Running the right schemas over the right resource.

use std::collections::HashMap;

use super::nvdl;
use super::preprocess::{self, DocumentKind};
use super::relaxng::derive::Validator;
use super::relaxng::pattern::{Arena, PatternId};
use super::relaxng::rng::{Compiler, MapResolver, join_relative};
use super::schema;
use super::schematron;
use super::tree::Document;

/// What a schema said about one place in a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// 1-based line in the source file.
    pub line: u32,
    pub severity: schematron::Severity,
    pub text: String,
}

/// The kind of resource being validated, which together with the EPUB version
/// picks the schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// `META-INF/container.xml`.
    Container,
    /// `META-INF/encryption.xml`.
    Encryption,
    /// `META-INF/metadata.xml`.
    Metadata,
    /// The package document.
    Package,
    Xhtml,
    /// An XHTML content document carrying the `nav` property, which has a
    /// grammar of its own on top of the XHTML one.
    Nav,
    Svg,
    Ncx,
    MediaOverlay,
}

impl ResourceKind {
    /// The kind of a manifest item, or `None` for one no schema covers.
    pub fn of(media_type: &str) -> Option<ResourceKind> {
        Some(match media_type {
            "application/xhtml+xml" => ResourceKind::Xhtml,
            "image/svg+xml" => ResourceKind::Svg,
            "application/x-dtbncx+xml" => ResourceKind::Ncx,
            "application/smil+xml" => ResourceKind::MediaOverlay,
            "application/oebps-package+xml" => ResourceKind::Package,
            _ => return None,
        })
    }

    /// The schemas epubcheck runs, in the order `ValidatorMap` lists them. A
    /// `.nvdl` entry dispatches to further schemas; the others are terminal.
    fn schemas(self, epub3: bool) -> &'static [&'static str] {
        match (self, epub3) {
            (ResourceKind::Container, false) => &["20/rng/container.rng"],
            (ResourceKind::Container, true) => &[
                "30/ocf-container-30.rnc",
                "30/multiple-renditions/container.sch",
            ],
            (ResourceKind::Encryption, false) => &["20/rng/encryption.rng"],
            (ResourceKind::Encryption, true) => {
                &["30/ocf-encryption-30.rnc", "30/ocf-encryption-30.sch"]
            }
            // There is no EPUB 2 `META-INF/metadata.xml` schema.
            (ResourceKind::Metadata, false) => &[],
            (ResourceKind::Metadata, true) => &["30/ocf-metadata-30.rnc", "30/ocf-metadata-30.sch"],
            (ResourceKind::Package, false) => &["20/rng/opf.rng", "20/sch/opf.sch"],
            // The five collection assertion sets are unconditional in EPUB 3:
            // each keys on a `<collection>` role, so a package without
            // collections simply matches no rule.
            (ResourceKind::Package, true) => &[
                "30/package-30.rnc",
                "30/package-30.sch",
                "30/collection-do-30.sch",
                "30/dict/dict-collection.sch",
                "30/idx/idx-collection.sch",
                "30/collection-manifest-30.sch",
                "30/previews/preview-collection.sch",
            ],
            (ResourceKind::Xhtml, false) => &[
                "20/rng/ops20.nvdl",
                "20/sch/xhtml.sch",
                "20/sch/id-unique.sch",
            ],
            (ResourceKind::Xhtml, true) => &["30/epub-xhtml-30.nvdl"],
            // A navigation document takes the navigation grammar in place of the XHTML one,
            // and both assertion sets. The version does not gate it (see NAV-001).
            (ResourceKind::Nav, _) => &[
                "30/epub-nav-30.rnc",
                "30/epub-xhtml-30.sch",
                "30/epub-nav-30.sch",
            ],
            (ResourceKind::Svg, false) => &["20/rng/ops20-svg.nvdl", "20/sch/id-unique.sch"],
            (ResourceKind::Svg, true) => &["30/epub-svg-30.nvdl"],
            (ResourceKind::Ncx, _) => &["20/rng/ncx.rng", "20/sch/ncx.sch"],
            // Media overlays are an EPUB 3 feature; there is no EPUB 2 schema.
            (ResourceKind::MediaOverlay, false) => &[],
            (ResourceKind::MediaOverlay, true) => {
                &["30/media-overlay-30.rnc", "30/media-overlay-30.sch"]
            }
        }
    }

    /// How [`preprocess`] should treat the document. Only the two content
    /// document types are rewritten before validation.
    fn preprocess_as(self) -> Option<DocumentKind> {
        match self {
            ResourceKind::Xhtml | ResourceKind::Nav => Some(DocumentKind::Xhtml),
            ResourceKind::Svg => Some(DocumentKind::Svg),
            _ => None,
        }
    }
}

/// Compiled schemas, cached across one book's resources.
#[derive(Default)]
pub struct Engine {
    grammars: HashMap<String, Option<(Arena, PatternId)>>,
    assertions: HashMap<String, Option<schematron::Schema>>,
    dispatch: HashMap<String, Option<nvdl::Rules>>,
}

impl Engine {
    pub fn new() -> Engine {
        Engine::default()
    }

    /// Validate one already-parsed document.
    ///
    /// A schema that will not compile is a defect in this port, not in the book:
    pub fn validate(&mut self, kind: ResourceKind, epub3: bool, text: &str) -> Vec<Message> {
        let Ok(mut doc) = Document::parse(text) else {
            // Well-formedness is reported elsewhere, and a document that does
            // not parse cannot be schema-validated.
            return Vec::new();
        };
        // EPUB 3 only, and before any schema sees the document — the grammars
        // are written against the rewritten form.
        if epub3 && let Some(as_kind) = kind.preprocess_as() {
            preprocess::preprocess(&mut doc, as_kind);
        }
        let mut out = Vec::new();
        for path in kind.schemas(epub3) {
            self.run(path, &doc, &mut out);
        }
        out
    }

    /// The notes [`preprocess`] makes on its way past, which are findings in
    /// their own right rather than schema violations.
    pub fn preprocess_notes(kind: ResourceKind, epub3: bool, text: &str) -> Vec<preprocess::Note> {
        if !epub3 {
            return Vec::new();
        }
        let (Some(as_kind), Ok(mut doc)) = (kind.preprocess_as(), Document::parse(text)) else {
            return Vec::new();
        };
        preprocess::preprocess(&mut doc, as_kind)
    }

    /// Run one schema, following an NVDL script to the schemas it names.
    fn run(&mut self, path: &str, doc: &Document, out: &mut Vec<Message>) {
        if path.ends_with(".nvdl") {
            let Some(rules) = self.rules(path) else {
                return;
            };
            let dispatch = rules.dispatch(doc);
            for node in &dispatch.rejected {
                out.push(Message {
                    line: doc.line(*node),
                    severity: schematron::Severity::Error,
                    text: format!(
                        "element \"{}\" is not allowed here",
                        doc.element(*node)
                            .map(|e| e.name.local.as_str())
                            .unwrap_or("?")
                    ),
                });
            }
            // Each section is a document in its own right, built by the script.
            let sections: Vec<(String, Document)> = dispatch
                .sections
                .into_iter()
                .map(|s| (s.schema.0, s.document))
                .collect();
            for (schema_path, section) in sections {
                self.run(&schema_path, &section, out);
            }
            return;
        }
        if path.ends_with(".sch") {
            let Some(schema) = self.schema(path) else {
                return;
            };
            for violation in schema.validate(doc) {
                out.push(Message {
                    line: violation.line,
                    severity: violation.severity,
                    text: violation.message,
                });
            }
            return;
        }
        // A `.rng` or `.rnc` grammar. The arena has to be borrowed mutably for
        // the derivative caches, so the lookup and the run cannot overlap.
        if !self.grammars.contains_key(path) {
            let compiled = compile_grammar(path);
            self.grammars.insert(path.to_string(), compiled);
        }
        let Some(Some((arena, start))) = self.grammars.get_mut(path) else {
            return;
        };
        let start = *start;
        for violation in Validator::new(arena).validate(doc, start) {
            out.push(Message {
                line: doc.line(violation.node),
                severity: schematron::Severity::Error,
                text: violation.message,
            });
        }
    }

    fn rules(&mut self, path: &str) -> Option<&nvdl::Rules> {
        self.dispatch
            .entry(path.to_string())
            .or_insert_with(|| {
                let source = schema::get(path)?;
                nvdl::Rules::compile(path, source).ok()
            })
            .as_ref()
    }

    fn schema(&mut self, path: &str) -> Option<&schematron::Schema> {
        self.assertions
            .entry(path.to_string())
            .or_insert_with(|| {
                let source = schema::get(path)?;
                schematron::Schema::compile(path, source, &resolve_vendored).ok()
            })
            .as_ref()
    }
}

fn compile_grammar(path: &str) -> Option<(Arena, PatternId)> {
    let files = schema::files();
    let source = schema::get(path)?;
    let mut arena = Arena::new();
    let start = {
        let resolver = MapResolver(&files);
        Compiler::new(&mut arena, &resolver)
            .compile(path, source)
            .ok()?
    };
    Some((arena, start))
}

/// Resolve an `<include>` against the vendored schemas.
fn resolve_vendored(base: &str, href: &str) -> Option<(String, String)> {
    let path = join_relative(base, href);
    schema::get(&path).map(|content| (path, content.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errors(kind: ResourceKind, epub3: bool, text: &str) -> Vec<String> {
        Engine::new()
            .validate(kind, epub3, text)
            .into_iter()
            .filter(|m| m.severity == schematron::Severity::Error)
            .map(|m| m.text)
            .collect()
    }

    const XHTML3: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
  <head><title>t</title></head>
  <body>
    <section epub:type="chapter">
      <h1>Heading</h1>
      <p data-note="ok">Text with <a href="#a">a link</a>.</p>
      <p id="a">Target.</p>
    </section>
  </body>
</html>"##;

    const XHTML2: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>t</title></head>
  <body>
    <div><h1>Heading</h1><p>Text with <a href="#a">a link</a>.</p><p id="a">Target.</p></div>
  </body>
</html>"##;

    const OPF3: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">u</dc:identifier>
    <dc:title>T</dc:title>
    <dc:language>en</dc:language>
    <meta property="dcterms:modified">2020-01-01T00:00:00Z</meta>
  </metadata>
  <manifest><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/></manifest>
  <spine><itemref idref="nav"/></spine>
</package>"##;

    const OPF2: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:identifier id="uid">u</dc:identifier>
    <dc:title>T</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="c" href="c.xhtml" media-type="application/xhtml+xml"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
  </manifest>
  <spine toc="ncx"><itemref idref="c"/></spine>
</package>"##;

    const NAV: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
  <head><title>t</title></head>
  <body>
    <nav epub:type="toc"><ol><li><a href="c.xhtml">One</a></li></ol></nav>
  </body>
</html>"##;

    const NCX: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head><meta name="dtb:uid" content="u"/></head>
  <docTitle><text>T</text></docTitle>
  <navMap>
    <navPoint id="n1" playOrder="1"><navLabel><text>One</text></navLabel><content src="c.xhtml"/></navPoint>
    <navPoint id="n2" playOrder="2"><navLabel><text>Two</text></navLabel><content src="d.xhtml"/></navPoint>
  </navMap>
</ncx>"##;

    /// The defects the schemas are responsible for reporting on the `RSC-005`
    #[test]
    fn the_schemas_cover_what_the_hand_written_checks_replaced() {
        for (label, kind, epub3, base) in [
            ("xhtml3", ResourceKind::Xhtml, true, XHTML3),
            ("xhtml2", ResourceKind::Xhtml, false, XHTML2),
            ("opf3", ResourceKind::Package, true, OPF3),
            ("opf2", ResourceKind::Package, false, OPF2),
            ("nav", ResourceKind::Nav, true, NAV),
            ("ncx", ResourceKind::Ncx, false, NCX),
        ] {
            let found = errors(kind, epub3, base);
            assert!(found.is_empty(), "base {label} is not clean: {found:?}");
        }

        // (what it replaces, kind, epub3, base, needle → replacement, expected
        // substring of the message).
        let cases: &[(&str, ResourceKind, bool, &str, &str, &str, &str)] = &[
            // Package structure and metadata.
            (
                "MissingTitle",
                ResourceKind::Package,
                true,
                OPF3,
                "<dc:title>T</dc:title>",
                "",
                r#""metadata" is incomplete"#,
            ),
            (
                "MissingLanguage",
                ResourceKind::Package,
                true,
                OPF3,
                "<dc:language>en</dc:language>",
                "",
                r#""metadata" is incomplete"#,
            ),
            (
                "MissingIdentifier",
                ResourceKind::Package,
                true,
                OPF3,
                r#"<dc:identifier id="uid">u</dc:identifier>"#,
                "",
                r#""metadata" is incomplete"#,
            ),
            (
                "EmptyDcContent",
                ResourceKind::Package,
                true,
                OPF3,
                "<dc:title>T</dc:title>",
                "<dc:title></dc:title>",
                r#""title" is incomplete"#,
            ),
            (
                "UnresolvedUniqueIdentifier",
                ResourceKind::Package,
                true,
                OPF3,
                r#"unique-identifier="uid""#,
                r#"unique-identifier="nothing""#,
                "unique-identifier attribute does not resolve",
            ),
            (
                "PackageIncomplete (manifest)",
                ResourceKind::Package,
                true,
                OPF3,
                r#"<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>"#,
                "",
                r#""manifest" is incomplete"#,
            ),
            (
                "PackageIncomplete (spine)",
                ResourceKind::Package,
                true,
                OPF3,
                r#"<itemref idref="nav"/>"#,
                "",
                r#""spine" is incomplete"#,
            ),
            (
                "SpineTocMissing",
                ResourceKind::Package,
                false,
                OPF2,
                r#"<spine toc="ncx">"#,
                "<spine>",
                r#""spine" is missing a required attribute"#,
            ),
            (
                "DisallowedOpfAttribute (EPUB 3 opf:role)",
                ResourceKind::Package,
                true,
                OPF3,
                "<dc:title>T</dc:title>",
                r#"<dc:title opf:role="aut" xmlns:opf="http://www.idpf.org/2007/opf">T</dc:title>"#,
                r#"attribute "role" is not allowed"#,
            ),
            (
                "DisallowedOpfAttribute (EPUB 2 spine page-map)",
                ResourceKind::Package,
                false,
                OPF2,
                r#"<spine toc="ncx">"#,
                r#"<spine toc="ncx" page-map="pm">"#,
                r#"attribute "page-map" is not allowed"#,
            ),
            (
                "IdNotNcName",
                ResourceKind::Package,
                true,
                OPF3,
                r#"id="uid""#,
                r#"id="1 bad""#,
                r#"value of attribute "id" is invalid"#,
            ),
            (
                "DuplicateId",
                ResourceKind::Package,
                true,
                OPF3,
                r#"<item id="nav""#,
                r#"<item id="uid""#,
                r#"Duplicate "uid""#,
            ),
            // Content documents.
            (
                "EmptyTitle",
                ResourceKind::Xhtml,
                true,
                XHTML3,
                "<title>t</title>",
                "<title></title>",
                r#"Element "title" must not be empty"#,
            ),
            (
                "MissingRequiredAttribute (img)",
                ResourceKind::Xhtml,
                false,
                XHTML2,
                "<h1>Heading</h1>",
                r#"<h1>Heading</h1><img src="x.png"/>"#,
                r#""img" is missing a required attribute"#,
            ),
            (
                "DisallowedContentAttribute",
                ResourceKind::Xhtml,
                false,
                XHTML2,
                r#"<p id="a">"#,
                r#"<p id="a" onclick="f()">"#,
                r#"attribute "onclick" is not allowed"#,
            ),
            (
                "UndeclaredContentElement",
                ResourceKind::Xhtml,
                false,
                XHTML2,
                "<h1>Heading</h1>",
                "<h1>Heading</h1><u>underlined</u>",
                r#"element "u" is not allowed here"#,
            ),
            (
                "MetaEncodingDeclaration",
                ResourceKind::Xhtml,
                true,
                XHTML3,
                "<title>t</title>",
                r#"<meta http-equiv="content-type" content="text/html; charset=iso-8859-1"/><title>t</title>"#,
                "encoding declaration state",
            ),
            (
                "BodyRoleNotAllowed",
                ResourceKind::Xhtml,
                true,
                XHTML3,
                "<body>",
                r#"<body role="doc-cover">"#,
                r#"value of attribute "role" is invalid"#,
            ),
            (
                "NestedHyperlink",
                ResourceKind::Xhtml,
                false,
                XHTML2,
                r##"<a href="#a">a link</a>"##,
                r##"<a href="#a"><a href="#b">nested</a></a>"##,
                "cannot contain any nested",
            ),
            // Navigation document and NCX.
            (
                "NavTocOccurrence",
                ResourceKind::Nav,
                true,
                NAV,
                r#"epub:type="toc""#,
                r#"epub:type="landmarks""#,
                r#"Exactly one "toc" nav element"#,
            ),
            (
                "NcxPlayOrder",
                ResourceKind::Ncx,
                false,
                NCX,
                r#"playOrder="2""#,
                r#"playOrder="7""#,
                "playOrder sequence has gaps",
            ),
        ];

        for (label, kind, epub3, base, needle, replacement, expected) in cases {
            assert!(
                base.contains(needle),
                "{label}: the fixture does not contain {needle:?}"
            );
            let mutated = base.replace(needle, replacement);
            let found = errors(*kind, *epub3, &mutated);
            assert!(
                found.iter().any(|m| m.contains(expected)),
                "{label}: expected a message naming {expected:?}, got {found:?}"
            );
        }
    }

    /// The whole stack over a realistic document, in both versions: NVDL picks
    /// the grammar, the grammar and the assertions both run, and a clean
    /// document produces nothing.
    #[test]
    fn a_valid_content_document_is_clean_in_both_versions() {
        let found = errors(ResourceKind::Xhtml, true, XHTML3);
        assert!(found.is_empty(), "EPUB 3: {found:?}");
        let found = errors(ResourceKind::Xhtml, false, XHTML2);
        assert!(found.is_empty(), "EPUB 2: {found:?}");
    }

    #[test]
    fn the_version_decides_which_grammar_judges() {
        // HTML5 sectioning is legal in EPUB 3 and absent from XHTML 1.1.
        assert!(
            errors(
                ResourceKind::Xhtml,
                false,
                XHTML3.replace("epub:type=\"chapter\"", "").as_str()
            )
            .iter()
            .any(|m| m.contains("section"))
        );
        // `data-*` reaches the grammar in EPUB 2 (nothing strips it) and is
        // stripped in EPUB 3 — the same document, opposite verdicts.
        assert!(
            errors(ResourceKind::Xhtml, true, XHTML3).is_empty(),
            "data-* is preprocessed away in EPUB 3"
        );
    }

    #[test]
    fn both_engines_report_through_the_same_call() {
        // A grammar violation and a Schematron violation in one document.
        let broken = XHTML2.replace(
            r##"<a href="#a">a link</a>"##,
            r##"<a href="#a"><a href="#b">nested</a></a><u>u</u>"##,
        );
        let found = errors(ResourceKind::Xhtml, false, &broken);
        assert!(
            found.iter().any(|m| m.contains('u')),
            "the grammar rejects <u>: {found:?}"
        );
        assert!(
            found.iter().any(|m| m.contains("nested")),
            "the assertions reject the nested <a>: {found:?}"
        );
    }

    #[test]
    fn a_package_document_is_judged_by_version() {
        let opf3 = r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
            <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
              <dc:identifier id="uid">u</dc:identifier><dc:title>T</dc:title>
              <dc:language>en</dc:language>
              <meta property="dcterms:modified">2020-01-01T00:00:00Z</meta>
            </metadata>
            <manifest><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/></manifest>
            <spine><itemref idref="nav"/></spine></package>"#;
        assert!(
            errors(ResourceKind::Package, true, opf3).is_empty(),
            "{:?}",
            errors(ResourceKind::Package, true, opf3)
        );
        // The same document under the EPUB 2 schemas: `properties` does not
        // exist there, and neither does version 3.0.
        assert!(!errors(ResourceKind::Package, false, opf3).is_empty());
    }

    #[test]
    fn a_line_number_survives_the_section_machinery() {
        // The message has to name the line in the *original* file, even though
        // NVDL handed the grammar a document it built itself.
        let broken = XHTML2.replace("<p>Text", "<u>bad</u><p>Text");
        let messages = Engine::new().validate(ResourceKind::Xhtml, false, &broken);
        let line = broken
            .lines()
            .position(|l| l.contains("<u>bad</u>"))
            .expect("the line exists") as u32
            + 1;
        assert!(
            messages.iter().any(|m| m.line == line),
            "expected a message on line {line}, got {messages:?}"
        );
    }

    #[test]
    fn every_resource_kind_has_schemas_that_compile() {
        use ResourceKind::*;
        for kind in [
            Container,
            Encryption,
            Metadata,
            Package,
            Xhtml,
            Nav,
            Svg,
            Ncx,
            MediaOverlay,
        ] {
            for epub3 in [false, true] {
                let mut engine = Engine::new();
                for path in kind.schemas(epub3) {
                    assert!(schema::get(path).is_some(), "{path} is not vendored");
                    // Compiling is what would fail; running it on an empty
                    // document only has to not panic.
                    engine.run(
                        path,
                        &Document::parse("<x/>").expect("parses"),
                        &mut Vec::new(),
                    );
                }
                for (path, compiled) in &engine.grammars {
                    assert!(compiled.is_some(), "{path} failed to compile");
                }
                for (path, compiled) in &engine.assertions {
                    assert!(compiled.is_some(), "{path} failed to compile");
                }
                for (path, compiled) in &engine.dispatch {
                    assert!(compiled.is_some(), "{path} failed to compile");
                }
            }
        }
    }

    #[test]
    fn the_preprocessing_notes_are_reported_separately() {
        let notes = Engine::preprocess_notes(
            ResourceKind::Xhtml,
            true,
            &XHTML3.replace(r#"data-note="ok""#, r#"data-Bad="x""#),
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
    }
}
