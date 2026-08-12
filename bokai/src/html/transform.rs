//! Transform ArenaDom to Chapter.

use super::arena::{ArenaDom, ArenaNodeData, ArenaNodeId};
use super::element_ref::ElementRef;
use super::{is_html_whitespace, is_html_whitespace_only};
use crate::model::role_map::element_to_role;
use crate::model::{Chapter, Node, NodeId, Role};
use crate::style::{
    CascadeIndex, CascadeScratch, ComputedStyle, Display, Origin, Stylesheet, WhiteSpace,
    compute_styles_indexed,
};

/// User agent stylesheet (browser defaults).
const UA_CSS: &str = include_str!("data/styles.css");

pub fn user_agent_stylesheet() -> Stylesheet {
    // The UA stylesheet is a constant, but `compile_html`/`compile_dom` runs
    // once per chapter, so re-parsing UA_CSS every time is pure redundant work.
    // Parse it once per thread and hand back a clone (cloning already-parsed
    // rules is far cheaper than re-tokenizing and re-parsing the CSS). Output is
    // unchanged — the cached sheet is a clone of the same parse.
    thread_local! {
        static UA_STYLESHEET: Stylesheet = Stylesheet::parse(UA_CSS);
    }
    UA_STYLESHEET.with(|ua| ua.clone())
}

/// Context for the transform operation.
struct TransformContext<'a> {
    dom: &'a ArenaDom,
    /// Rules bucketed by their rightmost selector, built once for the whole
    /// chapter so each element only tests candidates that could match it.
    cascade_index: CascadeIndex<'a>,
    /// Reusable candidate list + selector caches, shared across every element.
    cascade_scratch: CascadeScratch,
    chapter: Chapter,
    /// Map from ArenaNodeId to Chapter NodeId
    node_map: std::collections::HashMap<ArenaNodeId, NodeId>,
}

impl<'a> TransformContext<'a> {
    fn new(dom: &'a ArenaDom, stylesheets: &'a [(Stylesheet, Origin)]) -> Self {
        Self {
            dom,
            cascade_index: CascadeIndex::build(stylesheets),
            cascade_scratch: CascadeScratch::default(),
            chapter: Chapter::new(),
            node_map: std::collections::HashMap::new(),
        }
    }

    /// Transform the DOM to IR.
    fn transform(mut self) -> Chapter {
        // Find the body element, or use document root
        let body = self.dom.find_by_tag("body").unwrap_or(self.dom.document());

        // Get language from html element (if present) to propagate to all content
        let html_id_opt = self.dom.find_by_tag("html");
        let html_lang = html_id_opt.and_then(|html_id| {
            if let Some(node) = self.dom.get(html_id)
                && let ArenaNodeData::Element { attrs, .. } = &node.data
            {
                for attr in attrs {
                    if attr.name.local.as_ref() == "lang" && !attr.value.is_empty() {
                        return Some(attr.value.clone());
                    }
                }
            }
            None
        });

        // Compute the html element's style first so its inherited properties
        // (writing-mode, lang, etc.) become the parent context for body. Many
        // Japanese EPUBs put `class="vrtl"` on <html> with vertical-rl writing-mode;
        // without this step body's cascade never sees that class.
        let html_style = html_id_opt.map(|html_id| {
            let elem_ref = ElementRef::new(self.dom, html_id);
            compute_styles_indexed(
                elem_ref,
                &self.cascade_index,
                None,
                &mut self.chapter.styles,
                &mut self.cascade_scratch,
            )
        });

        // Compute body's style so its properties (like hyphens: auto) are inherited
        let mut body_style = {
            let elem_ref = ElementRef::new(self.dom, body);
            compute_styles_indexed(
                elem_ref,
                &self.cascade_index,
                html_style.as_ref(),
                &mut self.chapter.styles,
                &mut self.cascade_scratch,
            )
        };

        // Add html lang to body style if present (so it's inherited by all content)
        if let Some(lang) = html_lang
            && body_style.language.is_none()
        {
            body_style.language = Some(lang);
        }

        // Promote body's `id` attribute to the chapter root so anchors that
        // target it (e.g. NCX `<content src="ch.xhtml#bodyid"/>`) resolve.
        // Calibre-generated EPUBs frequently put a unique id on `<body>` and
        // reference it from the TOC; without this, `resolve_href` finds no
        // matching IR node and the TOC entry is silently dropped. The KFX
        // export side then falls back to chapter position when the target
        // node is ROOT (see `resolve_toc_target` in export/kfx.rs).
        if let Some(node) = self.dom.get(body)
            && let ArenaNodeData::Element { attrs, .. } = &node.data
        {
            for attr in attrs {
                if attr.name.local.as_ref() == "id" && !attr.value.is_empty() {
                    self.chapter.semantics.set_id(NodeId::ROOT, &attr.value);
                    break;
                }
            }
        }

        // The IR root *is* `<body>`, so it carries body's own computed style,
        // not just the inheritable part of it. A page-level declaration that
        // inherits into nothing — `body { background: url(…) }` painting a
        // page texture is the case in the wild — was computed here and then
        // dropped on the floor.
        let root_style = self.chapter.styles.intern(body_style.clone());
        if let Some(root) = self.chapter.node_mut(NodeId::ROOT) {
            root.style = root_style;
        }

        // Process body's children as children of IR root, inheriting body's style
        self.process_children(body, NodeId::ROOT, Some(&body_style));

        self.chapter
    }

    /// Process children of a DOM node.
    fn process_children(
        &mut self,
        dom_parent: ArenaNodeId,
        ir_parent: NodeId,
        parent_style: Option<&ComputedStyle>,
    ) {
        for child_id in self.dom.children(dom_parent).collect::<Vec<_>>() {
            self.process_node(child_id, ir_parent, parent_style);
        }
    }

    /// Process a single DOM node.
    fn process_node(
        &mut self,
        dom_id: ArenaNodeId,
        ir_parent: NodeId,
        parent_style: Option<&ComputedStyle>,
    ) {
        let node = match self.dom.get(dom_id) {
            Some(n) => n,
            None => return,
        };

        match &node.data {
            ArenaNodeData::Text(text) => {
                if is_html_whitespace_only(text) {
                    // Whitespace between inline elements should be preserved as a single space.
                    // We preserve whitespace unless:
                    // 1. We're at the root level (no parent style)
                    // 2. The whitespace contains newlines and we're in a block context
                    //
                    // This handles cases like: <cite><abbr>A</abbr> <abbr>B</abbr></cite>
                    // where the space between abbrs must be preserved even though cite is block.
                    let has_newlines = text.contains('\n');
                    let is_block_parent = parent_style
                        .map(|s| s.display != Display::Inline)
                        .unwrap_or(true);

                    // Skip pure-whitespace with newlines in block contexts (inter-element whitespace)
                    // But preserve spaces without newlines (intra-line whitespace between inline elements)
                    if has_newlines && is_block_parent {
                        return;
                    }

                    // No parent means we're at root level - skip whitespace
                    if parent_style.is_none() {
                        return;
                    }

                    // Preserve as a single space
                    let range = self.chapter.append_text(" ");
                    let text_node = Node::text(range);
                    let ir_id = self.chapter.alloc_node(text_node);
                    self.chapter.append_child(ir_parent, ir_id);
                    self.node_map.insert(dom_id, ir_id);
                    return;
                }

                // Check if whitespace should be preserved (pre, pre-wrap, pre-line)
                let preserve_whitespace = parent_style
                    .map(|s| {
                        matches!(
                            s.white_space,
                            WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::PreLine
                        )
                    })
                    .unwrap_or(false);

                // Normalize whitespace unless we're in a pre-like context
                let text_content = if preserve_whitespace {
                    text.to_string()
                } else {
                    normalize_whitespace(text)
                };
                let range = self.chapter.append_text(&text_content);
                // Text nodes don't have styles - they inherit from parent element
                let text_node = Node::text(range);
                let ir_id = self.chapter.alloc_node(text_node);
                self.chapter.append_child(ir_parent, ir_id);
                self.node_map.insert(dom_id, ir_id);
            }

            ArenaNodeData::Element { name, attrs, .. } => {
                // Compute style for this element
                let elem_ref = ElementRef::new(self.dom, dom_id);
                let mut computed = compute_styles_indexed(
                    elem_ref,
                    &self.cascade_index,
                    parent_style,
                    &mut self.chapter.styles,
                    &mut self.cascade_scratch,
                );

                // Merge lang attribute into style (for KFX language property)
                // This must happen before interning so the style includes the language
                for attr in attrs {
                    if attr.name.local.as_ref() == "lang" && !attr.value.is_empty() {
                        computed.language = Some(attr.value.to_string());
                        break;
                    }
                }

                // Map to role first (needed for Break check)
                let role = element_to_role(&name.local);

                // Skip hidden elements, but preserve Break nodes
                // CSS may hide <br> (e.g., in verse: "span + br { display: none }") but
                // we still need them for line breaks in the exported EPUB/KFX
                if computed.display == Display::None && role != Role::Break {
                    return;
                }

                // Create IR node
                let mut ir_node = Node::new(role);
                ir_node.style = self.chapter.styles.intern(computed.clone());

                let ir_id = self.chapter.alloc_node(ir_node);
                self.chapter.append_child(ir_parent, ir_id);
                self.node_map.insert(dom_id, ir_id);

                // Store semantic attributes
                for attr in attrs {
                    let attr_name = attr.name.local.as_ref();
                    let attr_ns = attr.name.ns.as_ref();
                    match attr_name {
                        // Core layout attributes. SVG `<image>` uses `href`
                        // (SVG2) or `xlink:href` (SVG1) for the image source;
                        // the IR's Image role expects `src`, so redirect when
                        // the node is an image. (HTML `<a href>` still routes
                        // through this arm for non-image roles.)
                        "href" if role == Role::Image => {
                            self.chapter.semantics.set_src(ir_id, &attr.value);
                        }
                        "href" => {
                            self.chapter.semantics.set_href(ir_id, &attr.value);
                        }
                        "src" => self.chapter.semantics.set_src(ir_id, &attr.value),
                        "alt" => self.chapter.semantics.set_alt(ir_id, &attr.value),
                        "id" => self.chapter.semantics.set_id(ir_id, &attr.value),
                        "title" => self.chapter.semantics.set_title(ir_id, &attr.value),
                        // Language (both lang and xml:lang)
                        "lang" => self.chapter.semantics.set_lang(ir_id, &attr.value),
                        // List start attribute (ol@start)
                        "start" if name.local.as_ref() == "ol" => {
                            if let Ok(start) = attr.value.parse::<u32>() {
                                self.chapter.semantics.set_list_start(ir_id, start);
                            }
                        }
                        // Semantic fidelity attributes
                        // epub:type attribute - handle both namespaced and prefixed forms
                        // html5ever parses "epub:type" as literal name with empty namespace
                        "type" if attr_ns == "http://www.idpf.org/2007/ops" => {
                            self.chapter.semantics.set_epub_type(ir_id, &attr.value);
                        }
                        "epub:type" => {
                            self.chapter.semantics.set_epub_type(ir_id, &attr.value);
                        }
                        "role" => {
                            self.chapter.semantics.set_aria_role(ir_id, &attr.value);
                        }
                        "datetime" => {
                            self.chapter.semantics.set_datetime(ir_id, &attr.value);
                        }
                        // Table cell attributes
                        "rowspan" if matches!(name.local.as_ref(), "td" | "th") => {
                            if let Ok(span) = attr.value.parse::<u32>() {
                                self.chapter.semantics.set_row_span(ir_id, span);
                            }
                        }
                        "colspan" if matches!(name.local.as_ref(), "td" | "th") => {
                            if let Ok(span) = attr.value.parse::<u32>() {
                                self.chapter.semantics.set_col_span(ir_id, span);
                            }
                        }
                        // `<col span>` / `<colgroup span>` is the same count
                        // in its own spelling: how many columns this entry
                        // describes.
                        "span" if matches!(name.local.as_ref(), "col" | "colgroup") => {
                            if let Ok(span) = attr.value.parse::<u32>() {
                                self.chapter.semantics.set_col_span(ir_id, span);
                            }
                        }
                        // Class attribute: store verbatim so EPUB → KFX → EPUB
                        // round-trips can preserve source class names instead
                        // of synthesizing `sN`. For code/pre we also extract
                        // the `language-xxx` / `lang-xxx` hint into the
                        // dedicated `language` semantic.
                        "class" => {
                            self.chapter.semantics.set_class(ir_id, &attr.value);
                            if matches!(name.local.as_ref(), "code" | "pre") {
                                for class in attr.value.split_whitespace() {
                                    if let Some(lang) = class.strip_prefix("language-") {
                                        self.chapter.semantics.set_language(ir_id, lang);
                                        break;
                                    }
                                    if let Some(lang) = class.strip_prefix("lang-") {
                                        self.chapter.semantics.set_language(ir_id, lang);
                                        break;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }

                // Mark th elements as header cells
                if name.local.as_ref() == "th" {
                    self.chapter.semantics.set_header_cell(ir_id, true);
                }

                // Process children
                self.process_children(dom_id, ir_id, Some(&computed));
            }

            // Skip other node types
            ArenaNodeData::Document | ArenaNodeData::Comment(_) | ArenaNodeData::Doctype { .. } => {
            }
        }
    }
}

/// Transform an ArenaDom to Chapter.
pub fn transform(dom: &ArenaDom, stylesheets: &[(Stylesheet, Origin)]) -> Chapter {
    let ctx = TransformContext::new(dom, stylesheets);
    ctx.transform()
}

fn normalize_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut prev_was_whitespace = false;

    for c in text.chars() {
        if is_html_whitespace(c) {
            if !prev_was_whitespace {
                result.push(' ');
                prev_was_whitespace = true;
            }
        } else {
            result.push(c);
            prev_was_whitespace = false;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use html5ever::driver::ParseOpts;
    use html5ever::parse_document;
    use html5ever::tendril::TendrilSink;

    use super::*;
    use crate::html::tree_sink::ArenaSink;

    fn parse_html(html: &str) -> ArenaDom {
        let sink = ArenaSink::new();
        let result = parse_document(sink, ParseOpts::default())
            .from_utf8()
            .one(html.as_bytes());
        result.into_dom()
    }

    #[test]
    fn test_basic_transform() {
        let dom = parse_html("<html><body><p>Hello, World!</p></body></html>");
        let ua = user_agent_stylesheet();
        let stylesheets = vec![(ua, Origin::UserAgent)];

        let chapter = transform(&dom, &stylesheets);

        // Should have root + paragraph (Text) + text content
        assert!(chapter.node_count() >= 3);

        // Find text nodes
        let mut found_text = false;
        for id in chapter.iter_dfs() {
            let node = chapter.node(id).unwrap();
            if node.role == Role::Text && !node.text.is_empty() {
                found_text = true;
                let text = chapter.text(node.text);
                assert!(text.contains("Hello"));
            }
        }
        assert!(found_text);
    }

    #[test]
    fn test_heading_levels() {
        let dom = parse_html("<html><body><h1>Title</h1><h2>Subtitle</h2></body></html>");
        let ua = user_agent_stylesheet();
        let stylesheets = vec![(ua, Origin::UserAgent)];

        let chapter = transform(&dom, &stylesheets);

        let mut h1_count = 0;
        let mut h2_count = 0;
        for id in chapter.iter_dfs() {
            match chapter.node(id).unwrap().role {
                Role::Heading(1) => h1_count += 1,
                Role::Heading(2) => h2_count += 1,
                _ => {}
            }
        }
        assert_eq!(h1_count, 1);
        assert_eq!(h2_count, 1);
    }

    #[test]
    fn test_link_semantics() {
        let dom = parse_html(r#"<a href="https://example.com">Link</a>"#);
        let ua = user_agent_stylesheet();
        let stylesheets = vec![(ua, Origin::UserAgent)];

        let chapter = transform(&dom, &stylesheets);

        // Find link node
        for id in chapter.iter_dfs() {
            if chapter.node(id).unwrap().role == Role::Link {
                assert_eq!(chapter.semantics.href(id), Some("https://example.com"));
                return;
            }
        }
        panic!("Link not found");
    }

    #[test]
    fn test_style_inheritance() {
        let dom = parse_html(
            r#"<html><body>
            <div style="color: red;"><p>Inherited</p></div>
        </body></html>"#,
        );
        let ua = user_agent_stylesheet();
        let author = Stylesheet::parse("div { color: red; }");
        let stylesheets = vec![(ua, Origin::UserAgent), (author, Origin::Author)];

        let chapter = transform(&dom, &stylesheets);

        // The paragraph should inherit the red color from div
        // (This is implicit in the cascade since we pass parent_style)
        assert!(chapter.node_count() > 1);
    }

    #[test]
    fn test_font_size_computed_through_cascade() {
        // Font sizes must come out of the cascade as computed px — the KFX
        // exporter emits flat styles with no element tree to resolve
        // relative units against. Covers a px span overriding an em heading
        // and relative sizes compounding through nesting.
        let dom = parse_html(
            r#"<html><body>
            <h1>Introduction:<span class="big">The Shadow</span></h1>
            <h2 class="scaled">Nested<span class="half">half</span></h2>
        </body></html>"#,
        );
        let ua = user_agent_stylesheet();
        let author = Stylesheet::parse(
            ".big { font-size: 24px; } .scaled { font-size: 2em; } .half { font-size: 50%; }",
        );
        let stylesheets = vec![(ua, Origin::UserAgent), (author, Origin::Author)];

        let chapter = transform(&dom, &stylesheets);

        // h1 text: UA 2em × 16px root
        assert_eq!(font_px_of(&chapter, "Introduction"), 32.0);
        // The 24px span overrides the inherited h1 size
        assert_eq!(font_px_of(&chapter, "The Shadow"), 24.0);
        // Author 2em on h2 resolves against body, and the nested 50% span
        // compounds against the h2's computed size
        assert_eq!(font_px_of(&chapter, "Nested"), 32.0);
        assert_eq!(font_px_of(&chapter, "half"), 16.0);
    }

    /// Computed font size (px) of the element enclosing the text `needle`.
    /// Text nodes carry the default style; the computed style lives on the
    /// enclosing element node.
    fn font_px_of(chapter: &Chapter, needle: &str) -> f32 {
        for id in chapter.iter_dfs() {
            let node = chapter.node(id).unwrap();
            if node.role == Role::Text && chapter.text(node.text).contains(needle) {
                let parent = node.parent.expect("text node has a parent element");
                let style_id = chapter.node(parent).unwrap().style;
                match chapter.styles.get(style_id).unwrap().font_size {
                    crate::style::Length::Px(v) => return v,
                    other => panic!("expected computed px for {needle:?}, got {other:?}"),
                }
            }
        }
        panic!("text {needle:?} not found");
    }

    #[test]
    fn test_style_attribute_participates_in_cascade() {
        let dom = parse_html(
            r#"<html><body>
            <p class="a" style="font-size: 24px">attr beats selector</p>
            <p style="font-size: 1.5em">relative attr<span>inherits</span></p>
        </body></html>"#,
        );
        let ua = user_agent_stylesheet();
        let author = Stylesheet::parse("p.a { font-size: 12px; }");
        let stylesheets = vec![(ua, Origin::UserAgent), (author, Origin::Author)];

        let chapter = transform(&dom, &stylesheets);

        // style="" outranks any selector specificity
        assert_eq!(font_px_of(&chapter, "attr beats selector"), 24.0);
        // Relative units in style="" resolve through the cascade and inherit
        assert_eq!(font_px_of(&chapter, "relative attr"), 24.0);
        assert_eq!(font_px_of(&chapter, "inherits"), 24.0);
    }

    #[test]
    fn test_important_beats_later_higher_specificity() {
        let dom = parse_html(r#"<html><body><p class="b">text</p></body></html>"#);
        let ua = user_agent_stylesheet();
        // The normal rule comes later AND has higher specificity — the
        // !important declaration must still win.
        let author =
            Stylesheet::parse(".b { font-size: 20px !important; } p.b { font-size: 10px; }");
        let stylesheets = vec![(ua, Origin::UserAgent), (author, Origin::Author)];

        let chapter = transform(&dom, &stylesheets);
        assert_eq!(font_px_of(&chapter, "text"), 20.0);
    }

    #[test]
    fn test_hidden_elements() {
        let dom = parse_html(
            r#"<html><head><title>Test</title></head><body><p>Visible</p></body></html>"#,
        );
        let ua = user_agent_stylesheet();
        let stylesheets = vec![(ua, Origin::UserAgent)];

        let chapter = transform(&dom, &stylesheets);

        // Should not contain title element (display: none)
        for id in chapter.iter_dfs() {
            let node = chapter.node(id).unwrap();
            if node.role == Role::Text {
                let text = chapter.text(node.text);
                assert!(!text.contains("Test"));
            }
        }
    }

    #[test]
    fn test_br_element() {
        let dom = parse_html(r#"<html><body><p>Line one<br/>Line two</p></body></html>"#);
        let ua = user_agent_stylesheet();
        let stylesheets = vec![(ua, Origin::UserAgent)];

        let chapter = transform(&dom, &stylesheets);

        // Should have a Break node
        let mut found_break = false;
        for id in chapter.iter_dfs() {
            if chapter.node(id).unwrap().role == Role::Break {
                found_break = true;
                break;
            }
        }
        assert!(found_break, "Break node not found");
    }

    #[test]
    fn test_br_element_xhtml_style() {
        // Test with XHTML-style self-closing br with namespace
        let dom = parse_html(
            r#"<?xml version="1.0" encoding="utf-8"?>
            <html xmlns="http://www.w3.org/1999/xhtml">
            <body><p><span>Line one</span><br/><span>Line two</span></p></body></html>"#,
        );
        let ua = user_agent_stylesheet();
        let stylesheets = vec![(ua, Origin::UserAgent)];

        let chapter = transform(&dom, &stylesheets);

        // Should have a Break node
        let mut found_break = false;
        for id in chapter.iter_dfs() {
            if chapter.node(id).unwrap().role == Role::Break {
                found_break = true;
                break;
            }
        }
        assert!(found_break, "Break node not found in XHTML-style input");
    }

    #[test]
    fn test_br_in_blockquote_verse() {
        // A blockquote of verse with <br/> line breaks between the lines.
        let dom = parse_html(
            r#"<html xmlns="http://www.w3.org/1999/xhtml">
            <body>
                <blockquote>
                    <p lang="la">
                        <span>Cui non conveniet sua res, ut calceus olim,</span>
                        <br/>
                        <span>Si pede major erit, subvertet; si minor, uret.</span>
                    </p>
                </blockquote>
            </body></html>"#,
        );
        let ua = user_agent_stylesheet();
        let stylesheets = vec![(ua, Origin::UserAgent)];

        let chapter = transform(&dom, &stylesheets);

        // Should have a Break node
        let mut found_break = false;
        for id in chapter.iter_dfs() {
            if chapter.node(id).unwrap().role == Role::Break {
                found_break = true;
                break;
            }
        }
        assert!(found_break, "Break node not found in blockquote verse");
    }
}
