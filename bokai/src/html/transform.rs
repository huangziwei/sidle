//! Transform ArenaDom to Chapter.

use super::arena::{ArenaDom, ArenaNodeData, ArenaNodeId};
use super::element_ref::ElementRef;
use super::is_html_whitespace;
use crate::model::role_map::element_to_role;
use crate::model::{Chapter, Node, NodeId, Role};
use crate::style::{
    CascadeIndex, CascadeScratch, ComputedStyle, Display, Origin, Stylesheet, WhiteSpace,
    compute_styles_indexed,
};

/// User agent stylesheet.
const UA_CSS: &str = include_str!("data/styles.css");

pub fn user_agent_stylesheet() -> Stylesheet {
    // `UA_CSS` is parsed once per thread and cloned for each call.
    thread_local! {
        static UA_STYLESHEET: Stylesheet = Stylesheet::parse(UA_CSS);
    }
    UA_STYLESHEET.with(|ua| ua.clone())
}

/// U+FFFC, the character an image or an inline-block counts as on its line.
const OBJECT: char = '\u{FFFC}';

/// The collapsible `' '` ending `node`'s text, kept or trimmed by what follows it.
#[derive(Clone, Copy)]
struct PendingSpace {
    node: NodeId,
    segment_break: bool,
}

/// White-space collapsing state across one inline formatting context.
#[derive(Clone, Copy, Default)]
struct InlineRun {
    /// The last character laid on the current line; `None` at a line start.
    last: Option<char>,
    pending: Option<PendingSpace>,
}

/// Context for the transform operation.
struct TransformContext<'a> {
    dom: &'a ArenaDom,
    /// Rules bucketed by their rightmost selector, built once per chapter.
    cascade_index: CascadeIndex<'a>,
    /// Reusable candidate list + selector caches, shared across every element.
    cascade_scratch: CascadeScratch,
    chapter: Chapter,
    /// Map from ArenaNodeId to Chapter NodeId
    node_map: std::collections::HashMap<ArenaNodeId, NodeId>,
    run: InlineRun,
}

impl<'a> TransformContext<'a> {
    fn new(dom: &'a ArenaDom, stylesheets: &'a [(Stylesheet, Origin)]) -> Self {
        Self {
            dom,
            cascade_index: CascadeIndex::build(stylesheets),
            cascade_scratch: CascadeScratch::default(),
            chapter: Chapter::new(),
            node_map: std::collections::HashMap::new(),
            run: InlineRun::default(),
        }
    }

    /// Transform the DOM to IR.
    fn transform(mut self) -> Chapter {
        // Find the body element, or use document root
        let body = self.dom.find_by_tag("body").unwrap_or(self.dom.document());

        // The `lang` attribute of `<html>`, if any.
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

        // `html_style` is the parent context `body_style` inherits from.
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

        // `body_style` is the parent context of every child of `body`.
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

        // `html_lang` applies where `body` states no language of its own.
        if let Some(lang) = html_lang
            && body_style.language.is_none()
        {
            body_style.language = Some(lang);
        }

        // `body`'s `id` lands on `NodeId::ROOT`, where a link to the body resolves.
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

        // `NodeId::ROOT` carries all of `body_style`, `background` and other
        // non-inherited properties included.
        let root_style = self.chapter.styles.intern(body_style.clone());
        if let Some(root) = self.chapter.node_mut(NodeId::ROOT) {
            root.style = root_style;
        }

        // Process body's children as children of IR root, inheriting body's style
        self.process_children(body, NodeId::ROOT, Some(&body_style));
        self.break_line();

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
                let Some(first) = text.chars().next() else {
                    return;
                };
                let preserve_whitespace = parent_style.is_some_and(|s| {
                    matches!(
                        s.white_space,
                        WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::PreLine
                    )
                });
                if preserve_whitespace {
                    self.settle(first);
                    self.run.last = text.chars().last().filter(|&c| c != '\n');
                    self.append_text_node(dom_id, ir_parent, text);
                    return;
                }
                let (content, trailing) = self.collapse(text);
                if content.is_empty() {
                    return;
                }
                let node = self.append_text_node(dom_id, ir_parent, &content);
                if let Some(segment_break) = trailing {
                    self.run.pending = Some(PendingSpace {
                        node,
                        segment_break,
                    });
                }
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

                // A `lang` attribute sets `computed.language`.
                for attr in attrs {
                    if attr.name.local.as_ref() == "lang" && !attr.value.is_empty() {
                        computed.language = Some(attr.value.to_string());
                        break;
                    }
                }

                let role = element_to_role(&name.local);

                // `display: none` drops an element, `Role::Break` excepted.
                if computed.display == Display::None && role != Role::Break {
                    return;
                }

                let line_edge = role == Role::Break
                    || matches!(
                        computed.display,
                        Display::Block | Display::ListItem | Display::TableCell | Display::TableRow
                    );
                let atomic = role == Role::Image || computed.display == Display::InlineBlock;
                if line_edge {
                    self.break_line();
                }
                if atomic {
                    self.settle(OBJECT);
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
                        // `href` on `Role::Image` is an SVG `<image>`'s source, `xlink:href` included.
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
                        // An item's own ordinal (li@value)
                        "value" if name.local.as_ref() == "li" => {
                            if let Ok(value) = attr.value.parse::<u32>() {
                                self.chapter.semantics.set_list_start(ir_id, value);
                            }
                        }
                        // Semantic fidelity attributes
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
                        // `span` on `col` / `colgroup`: the number of columns the entry describes.
                        "span" if matches!(name.local.as_ref(), "col" | "colgroup") => {
                            if let Ok(span) = attr.value.parse::<u32>() {
                                self.chapter.semantics.set_col_span(ir_id, span);
                            }
                        }
                        // `class` is stored verbatim.
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

                let own_run =
                    (atomic || role == Role::RubyText).then(|| std::mem::take(&mut self.run));
                self.process_children(dom_id, ir_id, Some(&computed));
                if let Some(outer) = own_run {
                    self.break_line();
                    self.run = outer;
                }
                if line_edge {
                    self.break_line();
                } else if atomic {
                    self.run.last = Some(OBJECT);
                }
            }

            // Skip other node types
            ArenaNodeData::Document | ArenaNodeData::Comment(_) | ArenaNodeData::Doctype { .. } => {
            }
        }
    }

    fn append_text_node(&mut self, dom_id: ArenaNodeId, ir_parent: NodeId, text: &str) -> NodeId {
        let range = self.chapter.append_text(text);
        let ir_id = self.chapter.alloc_node(Node::text(range));
        self.chapter.append_child(ir_parent, ir_id);
        self.node_map.insert(dom_id, ir_id);
        ir_id
    }

    /// Lay `text` under collapsible white space: the surviving text, and the
    /// segment-break flag of the run it ends on, if it ends on a space.
    fn collapse(&mut self, text: &str) -> (String, Option<bool>) {
        let mut out = String::with_capacity(text.len());
        let mut run: Option<bool> = None;
        for c in text.chars() {
            if is_html_whitespace(c) {
                run = Some(run.unwrap_or(false) || matches!(c, '\n' | '\r'));
                continue;
            }
            match run.take() {
                Some(segment_break) => self.space_before(&mut out, segment_break, c),
                None if out.is_empty() => self.settle(c),
                None => {}
            }
            out.push(c);
            self.run.last = Some(c);
        }
        let trailing = run.and_then(|segment_break| self.space_at_end(&mut out, segment_break));
        (out, trailing)
    }

    fn space_before(&mut self, out: &mut String, segment_break: bool, next: char) {
        if let Some(pending) = self.run.pending.as_mut() {
            pending.segment_break |= segment_break;
            self.settle(next);
            return;
        }
        let Some(prev) = self.run.last else {
            return;
        };
        if !(segment_break && removes_segment_break(prev, next)) {
            out.push(' ');
        }
    }

    fn space_at_end(&mut self, out: &mut String, segment_break: bool) -> Option<bool> {
        if let Some(pending) = self.run.pending.as_mut() {
            pending.segment_break |= segment_break;
            return None;
        }
        self.run.last?;
        out.push(' ');
        Some(segment_break)
    }

    /// Keep or trim the pending space, given `next` follows it.
    fn settle(&mut self, next: char) {
        let Some(pending) = self.run.pending.take() else {
            return;
        };
        if pending.segment_break
            && self
                .run
                .last
                .is_some_and(|prev| removes_segment_break(prev, next))
        {
            self.trim_pending(pending.node);
        }
    }

    fn break_line(&mut self) {
        if let Some(pending) = self.run.pending.take() {
            self.trim_pending(pending.node);
        }
        self.run.last = None;
    }

    fn trim_pending(&mut self, node: NodeId) {
        let Some(n) = self.chapter.node_mut(node) else {
            return;
        };
        n.text.len -= 1;
        if n.text.is_empty() {
            self.chapter.detach(node);
        }
    }
}

/// Transform an ArenaDom to Chapter.
pub fn transform(dom: &ArenaDom, stylesheets: &[(Stylesheet, Origin)]) -> Chapter {
    let ctx = TransformContext::new(dom, stylesheets);
    ctx.transform()
}

/// Whether a segment break between `before` and `after` is removed: beside
/// U+200B, or between two East Asian wide characters.
fn removes_segment_break(before: char, after: char) -> bool {
    before == '\u{200B}'
        || after == '\u{200B}'
        || (is_east_asian_wide(before) && is_east_asian_wide(after))
}

/// East Asian Width F, W or H in the CJK, kana, Bopomofo and Yi blocks and the
/// full/halfwidth forms, Hangul excluded.
fn is_east_asian_wide(c: char) -> bool {
    matches!(
        c,
        '\u{2E80}'..='\u{2FFF}'
            | '\u{3000}'..='\u{303E}'
            | '\u{3041}'..='\u{3096}'
            | '\u{3099}'..='\u{30FF}'
            | '\u{3105}'..='\u{312F}'
            | '\u{3190}'..='\u{31FF}'
            | '\u{3220}'..='\u{3247}'
            | '\u{3250}'..='\u{325F}'
            | '\u{327F}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{A000}'..='\u{A4CF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FE10}'..='\u{FE19}'
            | '\u{FE30}'..='\u{FE6F}'
            | '\u{FF01}'..='\u{FF9F}'
            | '\u{FFE0}'..='\u{FFE6}'
            | '\u{FFE8}'..='\u{FFEE}'
            | '\u{1AFF0}'..='\u{1B16F}'
            | '\u{1F200}'..='\u{1F2FF}'
            | '\u{20000}'..='\u{2FFFD}'
            | '\u{30000}'..='\u{3FFFD}'
    )
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

        // The paragraph inherits `color: red` from the div.
        assert!(chapter.node_count() > 1);
    }

    #[test]
    fn test_font_size_computed_through_cascade() {
        // Font sizes come out of the cascade as computed px.
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
        // 2em on h2 resolves against body; the nested 50% span halves it.
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
        // `!important` wins over a later rule of higher specificity.
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

    fn compile(body: &str) -> Chapter {
        let dom = parse_html(&format!("<html><body>{body}</body></html>"));
        transform(&dom, &[(user_agent_stylesheet(), Origin::UserAgent)])
    }

    /// Text of each top-level block, ruby annotations left out and breaks as `\n`.
    fn block_texts(chapter: &Chapter) -> Vec<String> {
        fn gather(chapter: &Chapter, id: NodeId, out: &mut String) {
            let node = chapter.node(id).unwrap();
            match node.role {
                Role::Text => out.push_str(chapter.text(node.text)),
                Role::RubyText => {}
                Role::Break => out.push('\n'),
                _ => {
                    for child in chapter.children(id) {
                        gather(chapter, child, out);
                    }
                }
            }
        }
        chapter
            .children(NodeId::ROOT)
            .map(|block| {
                let mut text = String::new();
                gather(chapter, block, &mut text);
                text
            })
            .collect()
    }

    #[test]
    fn collapsible_spaces_collapse_across_inlines_and_leave_line_edges() {
        let chapter = compile(concat!(
            "<p>a <span>\n  b</span></p>",
            "<p>\n  Lead and trail  \n</p>",
            "<p>\n<em>Hello</em>\n<em>world</em>\n</p>",
            "<div><em>a</em> <em>b</em></div>",
            "<p>a <br/>\n b</p>",
            r#"<p><img src="x.png"/> text</p>"#,
            r#"<div><img style="display: block" src="x.png"/>
              text</div>"#,
        ));
        assert_eq!(
            block_texts(&chapter),
            [
                "a b",
                "Lead and trail",
                "Hello world",
                "a b",
                "a\nb",
                " text",
                "text"
            ]
        );
    }

    #[test]
    fn segment_break_between_east_asian_characters_is_removed() {
        let chapter = compile(concat!(
            "<p>漢字\n漢字</p>",
            "<p>漢字 漢字</p>",
            "<p>word\nword</p>",
            "<p>行\u{200B}\nword</p>",
            "<p>先生に<ruby>\n    <rb>\n     訊\n    </rb><rt>\n き\n</rt></ruby>かれて</p>",
        ));
        assert_eq!(
            block_texts(&chapter),
            [
                "漢字漢字",
                "漢字 漢字",
                "word word",
                "行\u{200B}word",
                "先生に訊かれて"
            ]
        );
        let annotation = chapter
            .iter_dfs()
            .find(|&id| chapter.node(id).unwrap().role == Role::RubyText)
            .unwrap();
        let text: String = chapter
            .children(annotation)
            .map(|id| chapter.text(chapter.node(id).unwrap().text))
            .collect();
        assert_eq!(text, "き");
    }

    #[test]
    fn white_space_between_blocks_leaves_no_text() {
        let chapter = compile("<div>\n  <p>a</p>\n  <p>b</p>\n</div>\n<ul>\n  <li>c</li>\n</ul>\n");
        let blank = chapter
            .iter_dfs()
            .map(|id| chapter.node(id).unwrap())
            .filter(|n| n.role == Role::Text)
            .filter(|n| {
                chapter
                    .text(n.text)
                    .chars()
                    .all(|c| c.is_ascii_whitespace())
            })
            .count();
        assert_eq!(blank, 0);
    }

    #[test]
    fn link_rules_apply_and_state_rules_keep_their_selector_list() {
        let dom =
            parse_html(r#"<html><body><p class="y"><a href="x.xhtml">to</a></p></body></html>"#);
        let author = Stylesheet::parse("a:link { color: #0000ff } p:hover, .y { font-size: 20px }");
        let chapter = transform(
            &dom,
            &[
                (user_agent_stylesheet(), Origin::UserAgent),
                (author, Origin::Author),
            ],
        );
        assert_eq!(font_px_of(&chapter, "to"), 20.0);
        let link = chapter
            .iter_dfs()
            .find(|&id| chapter.node(id).unwrap().role == Role::Link)
            .unwrap();
        let color = chapter
            .styles
            .get(chapter.node(link).unwrap().style)
            .unwrap()
            .color;
        assert_eq!(color.map(|c| (c.r, c.g, c.b)), Some((0, 0, 255)));
    }

    #[test]
    fn inherited_break_properties_reach_nested_elements() {
        let dom = parse_html(
            r#"<html><body><div class="w"><p><span>text</span></p></div></body></html>"#,
        );
        let author = Stylesheet::parse(
            ".w { word-break: break-all; overflow-wrap: anywhere; orphans: 3; widows: 4 }",
        );
        let chapter = transform(
            &dom,
            &[
                (user_agent_stylesheet(), Origin::UserAgent),
                (author, Origin::Author),
            ],
        );
        let span = chapter
            .iter_dfs()
            .find(|&id| chapter.node(id).unwrap().role == Role::Inline)
            .unwrap();
        let style = chapter
            .styles
            .get(chapter.node(span).unwrap().style)
            .unwrap();
        assert_eq!(style.word_break, crate::style::WordBreak::BreakAll);
        assert_eq!(style.overflow_wrap, crate::style::OverflowWrap::Anywhere);
        assert_eq!((style.orphans, style.widows), (3, 4));
    }

    #[test]
    fn preserved_white_space_reaches_nested_elements() {
        let chapter = compile("<pre><code>  x\n    y</code></pre>");
        assert_eq!(block_texts(&chapter), ["  x\n    y"]);
    }
}
