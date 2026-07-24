//! A RELAX NG validator.
//!
//! epubcheck validates package documents and content documents against RELAX NG
//! grammars, so reproducing its `RSC-005` verdicts means running the same
//! grammars. The implementation follows James Clark's *An algorithm for RELAX NG
//! validation* — the derivative method, which needs no backtracking and handles
//! `interleave` (which a finite automaton cannot express) directly.
//!
//! - [`pattern`] — the simplified pattern model, hash-consed into an arena.
//! - [`datatype`] — the built-in and XSD datatype libraries the grammars use.
//! - [`derive`] — validation itself, by pattern derivative.
//! - [`rng`] — compiling a grammar written in the XML syntax (`.rng`).

pub mod datatype;
pub mod derive;
pub mod pattern;
pub mod rng;

#[cfg(test)]
mod tests {
    use super::derive::Validator;
    use super::pattern::Arena;
    use super::rng::{Compiler, MapResolver};
    use crate::validate::source::epub::xml::schema;
    use crate::validate::source::epub::xml::tree::Document;

    /// Compile one of epubcheck's own vendored grammars.
    fn grammar(path: &str) -> (Arena, super::pattern::PatternId) {
        let files = schema::files();
        let mut arena = Arena::new();
        let start = {
            let resolver = MapResolver(&files);
            let mut compiler = Compiler::new(&mut arena, &resolver);
            compiler
                .compile(path, schema::get(path).expect("vendored"))
                .unwrap_or_else(|e| panic!("{path} failed to compile: {e}"))
        };
        (arena, start)
    }

    fn errors(arena: &mut Arena, start: super::pattern::PatternId, xml: &str) -> Vec<String> {
        let doc = Document::parse(xml).expect("well-formed test document");
        Validator::new(arena)
            .validate(&doc, start)
            .into_iter()
            .map(|v| v.message)
            .collect()
    }

    const NCX: &str = r#"<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
        <head><meta name="dtb:uid" content="u"/></head>
        <docTitle><text>T</text></docTitle>
        <navMap><navPoint id="n1" playOrder="1">
          <navLabel><text>C1</text></navLabel><content src="c1.xhtml"/>
        </navPoint></navMap></ncx>"#;

    #[test]
    fn the_real_ncx_grammar_compiles_and_judges() {
        let (mut arena, start) = grammar("20/rng/ncx.rng");
        assert!(errors(&mut arena, start, NCX).is_empty(), "a valid NCX");

        // `navPoint` declares `id` as required — the rule book 300 trips.
        let no_id = NCX.replace(r#" id="n1""#, "");
        let found = errors(&mut arena, start, &no_id);
        assert!(!found.is_empty(), "navPoint without id must fail");

        // `navMap` requires at least one `navPoint`.
        let empty_map = NCX.replace(
            r#"<navMap><navPoint id="n1" playOrder="1">
          <navLabel><text>C1</text></navLabel><content src="c1.xhtml"/>
        </navPoint></navMap>"#,
            "<navMap></navMap>",
        );
        assert!(
            !errors(&mut arena, start, &empty_map).is_empty(),
            "empty navMap"
        );

        // An element the grammar declares nowhere.
        let alien = NCX.replace("<head>", "<head><bogus/>");
        assert!(
            !errors(&mut arena, start, &alien).is_empty(),
            "undeclared element"
        );
    }

    const OPF2: &str = r#"<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="uid">
        <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
          <dc:title>T</dc:title><dc:language>en</dc:language>
          <dc:identifier id="uid">u</dc:identifier>
        </metadata>
        <manifest><item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/></manifest>
        <spine toc="ncx"><itemref idref="c1"/></spine></package>"#;

    #[test]
    fn the_real_epub2_package_grammar_compiles_and_judges() {
        // `opf20.rng` has no <start> of its own — `opf.rng` is the entry point
        // epubcheck names, and it offers OPF 2.0 or the legacy OEB 1.2 package.
        let (mut arena, start) = grammar("20/rng/opf.rng");
        assert!(
            errors(&mut arena, start, OPF2).is_empty(),
            "a valid EPUB 2 OPF"
        );

        // The corpus classes this validator hand-ported, now decided by the
        // grammar itself: an EPUB-3 spine attribute, an `<item properties>`, and
        // an id that is not an NCName.
        for (label, xml) in [
            (
                "page-map on spine",
                OPF2.replace(r#"<spine toc="ncx">"#, r#"<spine toc="ncx" page-map="pm">"#),
            ),
            (
                "properties on item",
                OPF2.replace(
                    r#"media-type="application/xhtml+xml""#,
                    r#"media-type="application/xhtml+xml" properties="nav""#,
                ),
            ),
            (
                "id is not an NCName",
                OPF2.replace(r#"id="c1""#, r#"id="1c""#),
            ),
            (
                "empty guide",
                OPF2.replace("</package>", "<guide></guide></package>"),
            ),
        ] {
            assert!(
                !errors(&mut arena, start, &xml).is_empty(),
                "{label} must be rejected by the grammar"
            );
        }
    }

    /// The EPUB 2 content-document grammar: XHTML 1.1 assembled from ~25 module
    /// files plus the whole of SVG 1.1, and the grammar behind most of the
    /// corpus's RSC-005 findings.
    #[test]
    fn the_real_epub2_content_grammar_compiles_and_judges() {
        let (mut arena, start) = grammar("20/rng/content-xhtml.rng");
        let doc = |body: &str| {
            format!(
                r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>t</title></head><body>{body}</body></html>"#
            )
        };
        let found = errors(&mut arena, start, &doc("<p>text</p>"));
        assert!(
            found.is_empty(),
            "a plain XHTML 1.1 document must validate: {found:?}"
        );
        // Every one of these is a rule this validator hand-ported from the same
        // grammar; the engine decides them now.
        for (label, body) in [
            ("<u> is not in XHTML 1.1", "<p><u>x</u></p>"),
            ("HTML5 elements are absent", "<section><p>x</p></section>"),
            (
                "forms are not in the OPS subset",
                "<form action='x'></form>",
            ),
            ("value is not on li", "<ol><li value='3'>x</li></ol>"),
            (
                "width is not on td",
                "<table><tr><td width='50%'>x</td></tr></table>",
            ),
            ("img requires alt", "<p><img src='i.png'/></p>"),
            ("data-* is declared nowhere", "<p data-x='1'>x</p>"),
            ("role is declared nowhere", "<p role='main'>x</p>"),
        ] {
            assert!(
                !errors(&mut arena, start, &doc(body)).is_empty(),
                "{label}: {body} must be rejected"
            );
        }
        // A nested `<a>` is deliberately absent from that list: the grammar
        // permits it, and epubcheck rejects it through `20/sch/xhtml.sch`
        // instead — the Schematron half of this engine.
        assert!(
            errors(
                &mut arena,
                start,
                &doc("<p><a href='#a'><a href='#b'>x</a></a></p>")
            )
            .is_empty(),
            "nested <a> is a Schematron rule, not a grammar one"
        );

        // …and the constructs that ARE legal must stay legal.
        for (label, body) in [
            ("presentation module", "<p><big>x</big><tt>y</tt></p>"),
            ("tables", "<table><tr><td colspan='2'>x</td></tr></table>"),
            // SVG 1.1 requires width/height on <rect>, and the engine enforces it.
            (
                "inline svg",
                "<p><svg xmlns='http://www.w3.org/2000/svg'><rect width='1' height='1'/></svg></p>",
            ),
            ("img with alt", "<p><img src='i.png' alt=''/></p>"),
            ("style attribute", "<p style='color:red'>x</p>"),
        ] {
            let found = errors(&mut arena, start, &doc(body));
            assert!(found.is_empty(), "{label}: {body} is legal, got {found:?}");
        }
    }

    #[test]
    fn the_real_container_grammar_compiles_and_judges() {
        let (mut arena, start) = grammar("20/rng/container.rng");
        let ok = r#"<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
            <rootfiles><rootfile full-path="EPUB/package.opf"
              media-type="application/oebps-package+xml"/></rootfiles></container>"#;
        assert!(
            errors(&mut arena, start, ok).is_empty(),
            "a valid container"
        );
        let no_path = ok.replace(r#"full-path="EPUB/package.opf""#, "");
        assert!(
            !errors(&mut arena, start, &no_path).is_empty(),
            "full-path is required"
        );
    }
}
