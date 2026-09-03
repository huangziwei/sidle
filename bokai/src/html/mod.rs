//! HTML to IR compiler pipeline.

mod arena;
pub mod element_ref;
pub mod optimize;
pub mod panels;
mod transform;
mod tree_sink;

// CSS Text 3 §3 collapsible whitespace; `char::is_whitespace` also matches
// U+3000 IDEOGRAPHIC SPACE, the JP paragraph indent.
pub(crate) fn is_html_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0C' | '\r')
}

pub(crate) fn is_html_whitespace_only(text: &str) -> bool {
    text.chars().all(is_html_whitespace)
}

pub use arena::{ArenaDom, ArenaNode, ArenaNodeData, ArenaNodeId};
pub(crate) use element_ref::any_element_matches;
pub use element_ref::{BokoSelectors, ElementRef};
pub use optimize::optimize;
pub use panels::parse_panels;
pub use transform::user_agent_stylesheet;

pub use crate::style::{Declaration, Origin, Specificity, Stylesheet};

use html5ever::driver::ParseOpts;
use html5ever::tendril::TendrilSink;

use crate::model::Chapter;
use crate::util::percent_decode;
use tree_sink::ArenaSink;

/// True if the first 500 bytes of `html` carry `<?xml` or `xmlns=`.
fn looks_like_xhtml(html: &str) -> bool {
    let end = html.floor_char_boundary(500);
    let prefix = &html[..end];
    prefix.contains("<?xml") || prefix.contains("xmlns=")
}

/// Parse HTML or XHTML into an `ArenaDom`.
pub(crate) fn parse_dom(html: &str) -> ArenaDom {
    if looks_like_xhtml(html) {
        let sink = ArenaSink::new();
        let result =
            xml5ever::driver::parse_document(sink, xml5ever::driver::XmlParseOpts::default())
                .from_utf8()
                .one(html.as_bytes());
        let dom = result.into_dom();

        // `dom` without a populated `body` falls through to html5ever.
        if let Some(body) = dom.find_by_tag("body")
            && dom.children(body).next().is_some()
        {
            return dom;
        }
    }

    let sink = ArenaSink::new();
    let result = html5ever::parse_document(sink, ParseOpts::default())
        .from_utf8()
        .one(html.as_bytes());
    result.into_dom()
}

/// Compile HTML content to IR.
pub fn compile_html(html: &str, author_stylesheets: &[(Stylesheet, Origin)]) -> Chapter {
    compile_dom(&parse_dom(html), author_stylesheets)
}

/// Compile a parsed DOM to IR.
pub(crate) fn compile_dom(dom: &ArenaDom, author_stylesheets: &[(Stylesheet, Origin)]) -> Chapter {
    let ua = transform::user_agent_stylesheet();
    let mut all_stylesheets: Vec<(Stylesheet, Origin)> = vec![(ua, Origin::UserAgent)];
    for (sheet, origin) in author_stylesheets {
        all_stylesheets.push((sheet.clone(), *origin));
    }

    let mut chapter = transform::transform(dom, &all_stylesheets);

    optimize::optimize(&mut chapter);

    chapter
}

/// Compile HTML bytes to IR.
pub fn compile_html_bytes(html: &[u8], author_stylesheets: &[(Stylesheet, Origin)]) -> Chapter {
    let hint_encoding = crate::util::extract_xml_encoding(html);

    let html_str = crate::util::decode_text(html, hint_encoding);

    compile_html(&html_str, author_stylesheets)
}

/// Extract stylesheet links and inline styles from HTML.
pub fn extract_stylesheets(html: &str) -> (Vec<String>, Vec<String>) {
    extract_stylesheets_from_dom(&parse_dom(html))
}

/// Extract stylesheet references from a parsed DOM.
pub(crate) fn extract_stylesheets_from_dom(dom: &ArenaDom) -> (Vec<String>, Vec<String>) {
    let mut linked = Vec::new();
    let mut inline = Vec::new();

    let mut stack = vec![dom.document()];
    while let Some(id) = stack.pop() {
        if let Some(node) = dom.get(id)
            && let ArenaNodeData::Element { name, attrs, .. } = &node.data
        {
            match name.local.as_ref() {
                "link" => {
                    let is_stylesheet = attrs
                        .iter()
                        .any(|a| a.name.local.as_ref() == "rel" && a.value == "stylesheet");
                    if is_stylesheet
                        && let Some(href) = attrs
                            .iter()
                            .find(|a| a.name.local.as_ref() == "href")
                            .map(|a| a.value.clone())
                    {
                        linked.push(href);
                    }
                }
                "style" => {
                    let mut text = String::new();
                    for child in dom.children(id) {
                        if let Some(t) = dom.text_content(child) {
                            text.push_str(t);
                        }
                    }
                    if !text.trim().is_empty() {
                        inline.push(text);
                    }
                }
                _ => {}
            }
        }

        // Reverse push keeps document order on pop.
        let children: Vec<_> = dom.children(id).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }

    (linked, inline)
}

/// Resolve `rel` against `base` by path components alone; no filesystem access.
pub fn resolve_path(base: &str, rel: &str) -> String {
    use std::path::{Component, Path};

    let rel_path = Path::new(rel);

    // A leading `/` names the archive root.
    if rel_path.has_root() {
        return rel.trim_start_matches('/').to_string();
    }

    if rel.contains("://") || rel.starts_with("data:") {
        return rel.to_string();
    }

    let base_path = Path::new(base);
    let mut stack: Vec<&str> = base_path
        .parent()
        .unwrap_or(Path::new(""))
        .components()
        .filter_map(|c| {
            if let Component::Normal(s) = c {
                s.to_str()
            } else {
                None
            }
        })
        .collect();

    for component in rel_path.components() {
        match component {
            Component::ParentDir => {
                stack.pop();
            }
            Component::Normal(c) => {
                if let Some(s) = c.to_str() {
                    stack.push(s);
                }
            }
            Component::CurDir => {}
            _ => {}
        }
    }

    stack.join("/")
}

pub fn inline_css_imports<F>(src: &str, base: &str, mut load: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    let mut out = String::with_capacity(src.len());
    let bytes = src.as_bytes();
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' && src[i..].to_ascii_lowercase().starts_with("@import") {
            let mut j = i + "@import".len();
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let parsed = if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
                quoted_url(src, j)
            } else if j + 4 <= bytes.len() && src[j..j + 4].eq_ignore_ascii_case("url(") {
                url_function(src, j)
            } else {
                None
            };
            if let Some((url, mut k)) = parsed {
                while k < bytes.len() && bytes[k] != b';' && bytes[k] != b'}' {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == b';' {
                    k += 1;
                }
                out.push_str(&src[copied..i]);
                let child = resolve_path(base, &percent_decode(url));
                if let Some(child_css) = load(&child) {
                    out.push_str(&child_css);
                    out.push('\n');
                }
                copied = k;
                i = k;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&src[copied..]);
    out
}

pub fn css_import_targets(src: &str, base: &str) -> Vec<String> {
    let mut targets = Vec::new();
    inline_css_imports(src, base, |child| {
        targets.push(child.to_string());
        None
    });
    targets
}

fn quoted_url(src: &str, q_pos: usize) -> Option<(&str, usize)> {
    let bytes = src.as_bytes();
    let quote = bytes[q_pos];
    let start = q_pos + 1;
    let end_rel = bytes[start..].iter().position(|&b| b == quote)?;
    Some((&src[start..start + end_rel], start + end_rel + 1))
}

fn url_function(src: &str, u_pos: usize) -> Option<(&str, usize)> {
    let bytes = src.as_bytes();
    let mut p = u_pos + 4;
    while p < bytes.len() && bytes[p].is_ascii_whitespace() {
        p += 1;
    }
    let (url, end) = if p < bytes.len() && (bytes[p] == b'"' || bytes[p] == b'\'') {
        quoted_url(src, p)?
    } else {
        let url_start = p;
        while p < bytes.len() && bytes[p] != b')' && !bytes[p].is_ascii_whitespace() {
            p += 1;
        }
        (&src[url_start..p], p)
    };
    let mut k = end;
    while k < bytes.len() && bytes[k].is_ascii_whitespace() {
        k += 1;
    }
    if k >= bytes.len() || bytes[k] != b')' {
        return None;
    }
    Some((url, k + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;

    #[test]
    fn normalizes_parent_dir_in_url() {
        let mut requested: Vec<String> = Vec::new();
        let out = inline_css_imports(
            r#"@import url("../Styles/style0007.css"); body {}"#,
            "OEBPS/Styles/style0011.css",
            |p| {
                requested.push(p.to_string());
                Some(".vrtl { writing-mode: vertical-rl }".into())
            },
        );
        assert_eq!(requested, vec!["OEBPS/Styles/style0007.css"]);
        assert!(out.contains(".vrtl"));
        assert!(out.contains("body {}"));
    }

    #[test]
    fn handles_bare_url_function_and_lists_targets() {
        let targets = css_import_targets(
            "@import url(flow0007.css);\n@import 'flow0008.css';",
            "OEBPS/Styles/flow0011.css",
        );
        assert_eq!(
            targets,
            vec!["OEBPS/Styles/flow0007.css", "OEBPS/Styles/flow0008.css"]
        );
    }

    #[test]
    fn test_compile_simple_html() {
        let html = "<html><body><p>Test paragraph</p></body></html>";
        let chapter = compile_html(html, &[]);

        assert!(chapter.node_count() >= 3);

        let mut found_text = false;
        for id in chapter.iter_dfs() {
            if chapter.node(id).unwrap().role == Role::Text {
                found_text = true;
            }
        }
        assert!(found_text);
    }

    #[test]
    fn test_compile_with_css() {
        let html = "<p class='highlight'>Styled</p>";
        let css = ".highlight { font-weight: bold; }";

        let author = Stylesheet::parse(css);
        let chapter = compile_html(html, &[(author, Origin::Author)]);

        for id in chapter.iter_dfs() {
            let node = chapter.node(id).unwrap();
            if node.role == Role::Paragraph {
                let style = chapter.styles.get(node.style).unwrap();
                if style.font_weight == crate::style::FontWeight::BOLD {
                    return; // Found the styled paragraph
                }
            }
        }
        panic!("Styled paragraph not found");
    }

    #[test]
    fn test_extract_stylesheets() {
        let html = r#"
            <html>
            <head>
                <link rel="stylesheet" href="styles.css">
                <link rel="stylesheet" href="theme.css">
                <style>p { color: red; }</style>
            </head>
            <body><p>Content</p></body>
            </html>
        "#;

        let (linked, inline) = extract_stylesheets(html);

        assert_eq!(linked.len(), 2);
        assert!(linked.contains(&"styles.css".to_string()));
        assert!(linked.contains(&"theme.css".to_string()));

        assert_eq!(inline.len(), 1);
        assert!(inline[0].contains("color: red"));
    }

    #[test]
    fn test_compile_html_bytes() {
        let html = b"<p>Bytes test</p>";
        let chapter = compile_html_bytes(html, &[]);

        assert!(chapter.node_count() > 1);
    }

    #[test]
    fn test_resolve_path_parent_dir() {
        assert_eq!(
            resolve_path("OEBPS/text/ch1.html", "../images/logo.png"),
            "OEBPS/images/logo.png"
        );
    }

    #[test]
    fn test_resolve_path_same_dir() {
        assert_eq!(
            resolve_path("OEBPS/content.html", "images/photo.jpg"),
            "OEBPS/images/photo.jpg"
        );
    }

    #[test]
    fn test_resolve_path_absolute() {
        assert_eq!(
            resolve_path("ch1.html", "/images/absolute.png"),
            "images/absolute.png"
        );
    }

    #[test]
    fn test_resolve_path_multiple_parent() {
        assert_eq!(
            resolve_path("a/b/c/file.html", "../../images/test.png"),
            "a/images/test.png"
        );
    }

    #[test]
    fn test_resolve_path_current_dir() {
        assert_eq!(
            resolve_path("OEBPS/ch1.html", "./images/test.png"),
            "OEBPS/images/test.png"
        );
    }

    #[test]
    fn test_optimizer_merges_sibling_text_nodes() {
        let html = r#"
            <html><body>
                <p>Hello, <b>World</b>!</p>
            </body></html>
        "#;
        let chapter = compile_html(html, &[]);

        let mut text_content = String::new();
        for id in chapter.iter_dfs() {
            let node = chapter.node(id).unwrap();
            if node.role == Role::Text && !node.text.is_empty() {
                text_content.push_str(chapter.text(node.text));
            }
        }

        assert!(
            text_content.contains("Hello"),
            "Missing 'Hello' in: {}",
            text_content
        );
        assert!(
            text_content.contains("World"),
            "Missing 'World' in: {}",
            text_content
        );
    }

    #[test]
    fn test_optimizer_preserves_tree_structure() {
        let html = r#"
            <html><body>
                <p>First paragraph</p>
                <p>Second paragraph</p>
            </body></html>
        "#;
        let chapter = compile_html(html, &[]);

        let mut text_content = String::new();
        for id in chapter.iter_dfs() {
            let node = chapter.node(id).unwrap();
            if node.role == Role::Text && !node.text.is_empty() {
                text_content.push_str(chapter.text(node.text));
            }
        }

        assert!(
            text_content.contains("First paragraph"),
            "Missing 'First paragraph' in: {}",
            text_content
        );
        assert!(
            text_content.contains("Second paragraph"),
            "Missing 'Second paragraph' in: {}",
            text_content
        );
    }

    #[test]
    fn test_resolve_path_url_passthrough() {
        assert_eq!(
            resolve_path("ch1.html", "https://example.com/image.png"),
            "https://example.com/image.png"
        );
        assert_eq!(
            resolve_path("ch1.html", "data:image/png;base64,abc"),
            "data:image/png;base64,abc"
        );
    }

    #[test]
    fn test_br_survives_optimizer() {
        let chapter = compile_html(
            r#"<html xmlns="http://www.w3.org/1999/xhtml">
            <body>
                <blockquote>
                    <p>
                        <span>Line 1</span>
                        <br/>
                        <span>Line 2</span>
                    </p>
                </blockquote>
            </body></html>"#,
            &[],
        );

        let mut found_break = false;
        for id in chapter.iter_dfs() {
            if chapter.node(id).unwrap().role == Role::Break {
                found_break = true;
                break;
            }
        }
        assert!(found_break, "Break node lost during optimization");
    }

    #[test]
    fn test_xhtml_self_closing_script_preserves_content() {
        // html5ever reads a self-closing `<script/>` as an open script element.
        let html = r#"<html xmlns="http://www.w3.org/1999/xhtml">
            <head>
                <script src="book.js"/>
            </head>
            <body><p>Hello World</p></body>
        </html>"#;
        let chapter = compile_html(html, &[]);

        let mut found_text = false;
        for id in chapter.iter_dfs() {
            let node = chapter.node(id).unwrap();
            if node.role == Role::Text && !node.text.is_empty() {
                let text = chapter.text(node.text);
                if text.contains("Hello World") {
                    found_text = true;
                }
            }
        }
        assert!(
            found_text,
            "Self-closing <script/> in XHTML swallowed body content"
        );
    }

    #[test]
    fn test_looks_like_xhtml() {
        assert!(looks_like_xhtml(
            r#"<?xml version="1.0"?><html><body>Hi</body></html>"#
        ));
        assert!(looks_like_xhtml(
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>Hi</body></html>"#
        ));
        assert!(!looks_like_xhtml(
            "<html><body><p>Plain HTML</p></body></html>"
        ));
    }

    #[test]
    fn test_plain_html_still_works() {
        let html = "<html><body><p>Plain HTML</p></body></html>";
        let chapter = compile_html(html, &[]);

        let mut found_text = false;
        for id in chapter.iter_dfs() {
            let node = chapter.node(id).unwrap();
            if node.role == Role::Text && !node.text.is_empty() {
                let text = chapter.text(node.text);
                if text.contains("Plain HTML") {
                    found_text = true;
                }
            }
        }
        assert!(found_text, "Plain HTML content should be preserved");
    }

    #[test]
    fn test_xhtml_extract_stylesheets() {
        let html = r#"<html xmlns="http://www.w3.org/1999/xhtml">
            <head>
                <link rel="stylesheet" href="style.css"/>
                <script src="book.js"/>
                <style>p { color: red; }</style>
            </head>
            <body><p>Content</p></body>
        </html>"#;

        let (linked, inline) = extract_stylesheets(html);
        assert_eq!(linked.len(), 1);
        assert!(linked.contains(&"style.css".to_string()));
        assert_eq!(inline.len(), 1);
        assert!(inline[0].contains("color: red"));
    }
}
