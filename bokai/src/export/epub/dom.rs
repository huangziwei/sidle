//! Shared mutable XHTML DOM + chapter-document passes for the KFX→EPUB
//! engines.
//!
//! Port of just enough lxml.etree to host the calibre content.py logic. Both
//! KFX→EPUB routes build chapters through this one module — the mechanical
//! `kfx_to_epub` walk and the IR route's normalized export — so the emitted
//! XHTML (serialization shape, consolidation, attribute finalization) is
//! byte-identical by construction rather than by parallel implementation.
//! Permanent: it stays the DOM-synthesis regime's DOM ([`super::dom_synth`])
//! after the mechanical route retires.
//!
//! Nodes have either text content (mixed text + children, lxml-style) or a
//! tag with attributes. Attribute order is insertion order; `style` and
//! `class` are stored as regular attributes for simplicity.

use std::collections::HashMap;

use crate::style::CssDecl;

pub type NodeId = usize;

/// KFX `$761 layout_hints` + `$790 heading_level` pending per node — drives
/// the `<div>` → `<h<N>>` promotion in [`consolidate_part`] and blocks the
/// bare-div collapse (calibre treats `-kfx-*` sentinels as attributes).
pub type LayoutHints = HashMap<NodeId, (Vec<String>, Option<String>)>;

#[derive(Debug, Clone)]
pub struct Element {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    /// `lxml` style: element has optional leading text and each child has
    /// optional trailing "tail" text.
    pub text: Option<String>,
    pub tail: Option<String>,
    pub children: Vec<NodeId>,
    pub parent: Option<NodeId>,
}

impl Element {
    pub fn new(tag: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            attrs: Vec::new(),
            text: None,
            tail: None,
            children: Vec::new(),
            parent: None,
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let n = name.into();
        if let Some(slot) = self.attrs.iter_mut().find(|(k, _)| *k == n) {
            slot.1 = value.into();
        } else {
            self.attrs.push((n, value.into()));
        }
    }

    pub fn remove_attr(&mut self, name: &str) -> Option<String> {
        let pos = self.attrs.iter().position(|(k, _)| k == name)?;
        Some(self.attrs.remove(pos).1)
    }

    pub fn has_attr(&self, name: &str) -> bool {
        self.attrs.iter().any(|(k, _)| k == name)
    }
}

#[derive(Debug)]
pub struct Dom {
    nodes: Vec<Element>,
    pub root: NodeId,
}

impl Dom {
    /// Create a new DOM rooted at an `<html xmlns=...>` element.
    pub fn new_xhtml() -> Self {
        let mut nodes = Vec::new();
        let mut html = Element::new("html");
        html.set("xmlns", "http://www.w3.org/1999/xhtml");
        nodes.push(html);
        Self { nodes, root: 0 }
    }

    pub fn create_element(&mut self, tag: impl Into<String>) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Element::new(tag));
        id
    }

    pub fn get(&self, id: NodeId) -> &Element {
        &self.nodes[id]
    }

    pub fn get_mut(&mut self, id: NodeId) -> &mut Element {
        &mut self.nodes[id]
    }

    /// Append `child` to the end of `parent.children`.
    pub fn append(&mut self, parent: NodeId, child: NodeId) {
        self.nodes[child].parent = Some(parent);
        self.nodes[parent].children.push(child);
    }

    /// Insert `child` at `idx` in `parent.children`.
    pub fn insert(&mut self, parent: NodeId, idx: usize, child: NodeId) {
        self.nodes[child].parent = Some(parent);
        self.nodes[parent].children.insert(idx, child);
    }

    /// Move `src`'s attributes, text, and children onto `dst`, leaving `src`
    /// empty and detached. `dst` keeps its own tag and node id — for adopting
    /// a built element into a node other code already holds a reference to.
    /// Attributes `dst` already sets win; text is appended.
    pub fn move_into(&mut self, src: NodeId, dst: NodeId) {
        if src == dst {
            return;
        }
        let attrs = std::mem::take(&mut self.nodes[src].attrs);
        for (k, v) in attrs {
            if !self.nodes[dst].attrs.iter().any(|(n, _)| *n == k) {
                self.nodes[dst].attrs.push((k, v));
            }
        }
        if let Some(text) = self.nodes[src].text.take() {
            let slot = self.nodes[dst].text.get_or_insert_with(String::new);
            slot.push_str(&text);
        }
        let children = std::mem::take(&mut self.nodes[src].children);
        for child in children {
            self.append(dst, child);
        }
        // Detach `src` from whatever held it.
        if let Some(parent) = self.nodes[src].parent.take()
            && let Some(pos) = self.nodes[parent].children.iter().position(|&c| c == src)
        {
            self.nodes[parent].children.remove(pos);
        }
    }

    /// Convenience: create + append.
    pub fn sub_element(&mut self, parent: NodeId, tag: impl Into<String>) -> NodeId {
        let n = self.create_element(tag);
        self.append(parent, n);
        n
    }

    /// Set `el`'s inline text, expanding `'\n'` into `<br/>` children. KFX
    /// stores a hard line break (`<br>`) as a newline inside the text content
    /// (see the exporter's Break→`"\n"`); plain HTML would collapse it to a
    /// space, so a `罫囲み` box of `l1\nl2\nl3` would otherwise run together on
    /// one line. Segments are joined by `<br/>` (first → `text`, the rest →
    /// each `<br/>`'s `tail`). The common no-newline case just sets `text`.
    pub fn set_inline_text(&mut self, el: NodeId, text: &str) {
        let mut parts = text.split('\n');
        if let Some(first) = parts.next() {
            self.nodes[el].text = Some(first.to_string());
        }
        for part in parts {
            let br = self.sub_element(el, "br");
            self.nodes[br].tail = Some(part.to_string());
        }
    }

    /// Find `child`'s index in `parent.children`.
    pub fn child_index(&self, parent: NodeId, child: NodeId) -> Option<usize> {
        self.nodes[parent].children.iter().position(|&c| c == child)
    }

    /// Remove `child` from its parent. Doesn't free the node.
    pub fn remove_from_parent(&mut self, child: NodeId) {
        if let Some(parent) = self.nodes[child].parent {
            if let Some(idx) = self.child_index(parent, child) {
                self.nodes[parent].children.remove(idx);
            }
            self.nodes[child].parent = None;
        }
    }

    /// Replace `old`'s position in its parent with `new`. Both must exist.
    pub fn replace(&mut self, old: NodeId, new: NodeId) {
        if let Some(parent) = self.nodes[old].parent
            && let Some(idx) = self.child_index(parent, old)
        {
            self.nodes[parent].children[idx] = new;
            self.nodes[new].parent = Some(parent);
            self.nodes[old].parent = None;
        }
    }

    /// Serialize the subtree rooted at `id` to an XHTML string.
    pub fn serialize(&self, id: NodeId) -> String {
        let mut s = String::new();
        self.write_node(id, &mut s);
        s
    }

    fn write_node(&self, id: NodeId, out: &mut String) {
        let e = &self.nodes[id];
        out.push('<');
        out.push_str(&e.tag);
        for (k, v) in &e.attrs {
            out.push(' ');
            out.push_str(k);
            out.push_str("=\"");
            xml_attr_escape(v, out);
            out.push('"');
        }
        let void = is_void_element(&e.tag);
        if void && e.children.is_empty() && e.text.is_none() {
            out.push_str("/>");
        } else {
            out.push('>');
            if let Some(t) = &e.text {
                xml_text_escape(t, out);
            }
            for &c in &e.children {
                self.write_node(c, out);
                if let Some(tail) = &self.nodes[c].tail {
                    xml_text_escape(tail, out);
                }
            }
            out.push_str("</");
            out.push_str(&e.tag);
            out.push('>');
        }
    }

    /// Number of nodes (debug / metrics).
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True when the DOM holds no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

fn is_void_element(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "source"
            | "track"
            | "wbr"
    )
}

fn xml_attr_escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

fn xml_text_escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

/// Build the top-level `<html><head><title>…</title></head><body>…</body></html>`
/// skeleton and return `(html_id, head_id, body_id)`.
pub fn new_book_part(title: &str) -> (Dom, NodeId, NodeId, NodeId) {
    let mut dom = Dom::new_xhtml();
    let html = dom.root;
    let head = dom.sub_element(html, "head");
    let title_el = dom.sub_element(head, "title");
    dom.get_mut(title_el).text = Some(title.to_string());
    let body = dom.sub_element(html, "body");
    (dom, html, head, body)
}

/// Helper map from node id → CSS class names. Used by the stylesheet
/// dedupe pass.
#[derive(Default)]
pub struct ClassMap {
    pub by_node: HashMap<NodeId, Vec<String>>,
}

/// Assemble the final chapter file: XML declaration + HTML5 doctype + the
/// serialized tree, exactly the byte shape both KFX→EPUB routes ship.
pub fn chapter_document(dom: &Dom) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html>\n{}",
        dom.serialize(dom.root)
    )
}

// ---------------------------------------------------------------------------
// Part-level passes (calibre `consolidate_html` + friends)
// ---------------------------------------------------------------------------

/// HTML block-level elements (calibre's set in
/// `yj_to_epub_properties.py:1965`). Used by [`consolidate_part`] to decide
/// whether a `<div>` qualifies as a leaf-text paragraph.
const BLOCK_TAGS: &[&str] = &[
    "aside",
    "body",
    "caption",
    "div",
    "figure",
    "footer",
    "header",
    "main",
    "nav",
    "section",
    "article",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "li",
    "ol",
    "ul",
    "dl",
    "dt",
    "dd",
    "p",
    "blockquote",
    "pre",
    "hr",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "td",
    "th",
    "figcaption",
];

fn is_block_tag(tag: &str) -> bool {
    BLOCK_TAGS.contains(&tag)
}

/// Calibre's `is_inline_only` (`yj_to_epub_content.py:1900`): an element is
/// inline-only if it's an `<svg>`, or it's one of the inline tags
/// (a/audio/img/rb/rt/ruby/span/video) with every descendant inline-only. Used
/// by the `render:inline` demotion to decide whether a `<div>` can become a
/// `<span>` without swallowing a block child.
pub fn is_inline_only(dom: &Dom, id: NodeId) -> bool {
    let tag = dom.get(id).tag.as_str();
    if tag == "svg" {
        return true;
    }
    if !matches!(
        tag,
        "a" | "audio" | "img" | "rb" | "rt" | "ruby" | "span" | "video"
    ) {
        return false;
    }
    dom.get(id).children.iter().all(|&c| is_inline_only(dom, c))
}

/// Strip every `<span>` whose attribute list is empty (or carries only an
/// empty `class=""`), inlining its text and children into the parent.
/// Mirrors calibre's `consolidate_html` span pass (epub_output.py:783).
///
/// lxml semantics for `strip_tags`:
/// - `span.text` appends to previous-sibling.tail (or parent.text when
///   span is the first child),
/// - span's children move into span's position in parent.children, in
///   order,
/// - `span.tail` appends to the new last-child-of-span's tail (or the
///   previous tail-bearer when span had no children).
pub fn strip_empty_spans(dom: &mut Dom) {
    // Snapshot ids to iterate; the strip mutates parent.children but we
    // walk via a stable id list. Repeat until a pass produces zero strips,
    // since a stripped span may unwrap a nested empty span.
    loop {
        let mut stripped_any = false;
        for id in 0..dom.len() {
            let elem = dom.get(id);
            if elem.tag != "span" {
                continue;
            }
            // "Empty" = no attrs, OR only attrs that are noise (empty class).
            let has_meaningful_attr = elem.attrs.iter().any(|(k, v)| {
                if k == "class" {
                    !v.trim().is_empty()
                } else {
                    !v.is_empty() || !k.is_empty()
                }
            });
            if has_meaningful_attr {
                continue;
            }
            let Some(parent_id) = elem.parent else {
                continue;
            };
            let Some(pos) = dom.child_index(parent_id, id) else {
                continue;
            };
            // Pull the span's text + children + tail before mutating.
            let span_text = dom.get(id).text.clone().unwrap_or_default();
            let span_children: Vec<NodeId> = dom.get(id).children.clone();
            let span_tail = dom.get(id).tail.clone().unwrap_or_default();

            // 1. Splice span.text into the preceding text slot:
            //    - if span is first child: parent.text += span_text
            //    - else: prev-sibling.tail += span_text
            if !span_text.is_empty() {
                if pos == 0 {
                    let parent = dom.get_mut(parent_id);
                    let mut t = parent.text.clone().unwrap_or_default();
                    t.push_str(&span_text);
                    parent.text = Some(t);
                } else {
                    let prev_id = dom.get(parent_id).children[pos - 1];
                    let prev = dom.get_mut(prev_id);
                    let mut t = prev.tail.clone().unwrap_or_default();
                    t.push_str(&span_text);
                    prev.tail = Some(t);
                }
            }

            // 2. Remove span from parent.children, then insert span_children
            //    at the same pos (in order). Reparent each.
            {
                let parent = dom.get_mut(parent_id);
                parent.children.remove(pos);
                for (i, &child) in span_children.iter().enumerate() {
                    parent.children.insert(pos + i, child);
                }
            }
            for &child in &span_children {
                dom.get_mut(child).parent = Some(parent_id);
            }

            // 3. Splice span.tail. If span had children, append onto the
            //    last child's tail; else handle like the text case (onto
            //    the new previous sibling, or parent.text).
            if !span_tail.is_empty() {
                if let Some(&last) = span_children.last() {
                    let e = dom.get_mut(last);
                    let mut t = e.tail.clone().unwrap_or_default();
                    t.push_str(&span_tail);
                    e.tail = Some(t);
                } else if pos == 0 {
                    // No prev sibling, no inserted children — falls onto
                    // parent.text.
                    let parent = dom.get_mut(parent_id);
                    let mut t = parent.text.clone().unwrap_or_default();
                    t.push_str(&span_tail);
                    parent.text = Some(t);
                } else {
                    let prev_id = dom.get(parent_id).children[pos - 1];
                    let prev = dom.get_mut(prev_id);
                    let mut t = prev.tail.clone().unwrap_or_default();
                    t.push_str(&span_tail);
                    prev.tail = Some(t);
                }
            }

            // Orphan the stripped span (leave the node in the arena —
            // node ids are stable; nothing references it from a parent now).
            dom.get_mut(id).parent = None;
            dom.get_mut(id).children.clear();
            stripped_any = true;
        }
        if !stripped_any {
            break;
        }
    }
}

/// True when a `<div>` carries no attributes of any kind — directly on the DOM
/// node (e.g. an `id` stamped by the anchor pass) OR pending in the caller's
/// class / inline-style / layout-hint maps (not yet flushed to attrs at
/// consolidate time). Calibre's div-collapse only unwraps genuinely-empty
/// wrapper divs (`len(e.attrib) == 0`), and `-kfx-layout-hints` /
/// `-kfx-heading-level` count as attributes there.
fn div_is_bare(
    dom: &Dom,
    element_classes: &HashMap<NodeId, Vec<String>>,
    element_styles: &HashMap<NodeId, CssDecl>,
    element_layout_hints: &LayoutHints,
    id: NodeId,
) -> bool {
    dom.get(id).attrs.is_empty()
        && element_classes.get(&id).is_none_or(|c| c.is_empty())
        && element_styles.get(&id).is_none_or(|s| s.is_empty())
        && !element_layout_hints.contains_key(&id)
}

/// Unwrap `e` (an attribute-less div that is the SOLE child of `parent`, whose
/// `text` is empty) into `parent`: `e`'s leading text becomes the parent's
/// text, `e`'s children take `e`'s place, and `e`'s tail trails the last child
/// (lxml `strip_tags` semantics, specialised to the sole-child case).
fn unwrap_into_parent(dom: &mut Dom, e: NodeId, parent: NodeId) {
    let e_text = dom.get(e).text.clone();
    let e_children = dom.get(e).children.clone();
    let e_tail = dom.get(e).tail.clone();
    dom.get_mut(parent).text = e_text;
    dom.get_mut(parent).children = e_children.clone();
    for &c in &e_children {
        dom.get_mut(c).parent = Some(parent);
    }
    if let Some(tail) = e_tail.filter(|t| !t.is_empty()) {
        if let Some(&last) = e_children.last() {
            let cur = dom.get(last).tail.clone().unwrap_or_default();
            dom.get_mut(last).tail = Some(cur + &tail);
        } else {
            let cur = dom.get(parent).text.clone().unwrap_or_default();
            dom.get_mut(parent).text = Some(cur + &tail);
        }
    }
    dom.get_mut(e).parent = None;
    dom.get_mut(e).children.clear();
}

/// Port of calibre's `consolidate_html` div-collapse (`epub_output.py:792-814`).
/// Strips an attribute-less `<div>` that is the sole child of a block-level
/// parent (with no leading parent text), splicing its contents up into the
/// parent. KFX wraps content in redundant nested `<div>`s; without this, a
/// heading container holding a single bare wrapper `<div>` keeps a spurious
/// block child and never promotes to `<hN>`. Re-scans after each collapse
/// (one removal can expose another), matching calibre's `while True`.
fn collapse_redundant_divs(
    dom: &mut Dom,
    element_classes: &HashMap<NodeId, Vec<String>>,
    element_styles: &HashMap<NodeId, CssDecl>,
    element_layout_hints: &LayoutHints,
) {
    const BODY_CHILD_BLOCKS: &[&str] = &[
        "aside", "div", "figure", "h1", "h2", "h3", "h4", "h5", "h6", "hr", "iframe", "ol", "p",
        "table", "ul",
    ];
    loop {
        let mut target: Option<(NodeId, NodeId)> = None;
        for id in 0..dom.len() {
            if dom.get(id).tag != "div"
                || !div_is_bare(
                    dom,
                    element_classes,
                    element_styles,
                    element_layout_hints,
                    id,
                )
            {
                continue;
            }
            let Some(parent) = dom.get(id).parent else {
                continue;
            };
            if dom.get(parent).children.len() != 1
                || dom
                    .get(parent)
                    .text
                    .as_deref()
                    .is_some_and(|t| !t.is_empty())
            {
                continue;
            }
            let ptag = dom.get(parent).tag.as_str();
            let ok = match ptag {
                "aside" | "caption" | "div" | "figure" | "h1" | "h2" | "h3" | "h4" | "h5"
                | "h6" | "li" | "p" | "td" => true,
                "body" => {
                    let e = dom.get(id);
                    let e_text_empty = e.text.as_deref().is_none_or(|t| t.is_empty());
                    e_text_empty
                        && e.children.iter().all(|&c| {
                            BODY_CHILD_BLOCKS.contains(&dom.get(c).tag.as_str())
                                && dom.get(c).tail.as_deref().is_none_or(|t| t.is_empty())
                        })
                }
                _ => false,
            };
            if ok {
                target = Some((id, parent));
                break;
            }
        }
        let Some((e, parent)) = target else { break };
        unwrap_into_parent(dom, e, parent);
    }
}

/// Port of calibre's `consolidate_html` (epub_output.py:742) +
/// div→p promotion (yj_to_epub_properties.py:1921), one part at a time.
///
/// Four passes:
/// 1. Strip attribute-less `<span>` (and class-less ones) — merges their
///    text/children into the parent so the spine isn't 90% `<span>` noise.
/// 2. Collapse redundant single-child wrapper `<div>`s BEFORE computing
///    block/text descendants, so a heading container holding only a bare
///    wrapper div is seen as having no block child and can promote to `<hN>`.
/// 3. Rename leaf-text `<div>`s to `<p>` (no block child + has text).
/// 4. Promote `<div>` / `<p>` to `<h<N>>` for elements whose KFX style
///    carries `$761 layout_hints` containing `heading` (level from
///    `$790 yj.semantics.heading_level`, default 1). Figure promotion is
///    calibre-gated on `not epub2_desired` — we emit EPUB 2.0-compatible
///    chapters, so it's skipped.
pub fn consolidate_part(
    dom: &mut Dom,
    element_classes: &HashMap<NodeId, Vec<String>>,
    element_styles: &HashMap<NodeId, CssDecl>,
    element_layout_hints: &LayoutHints,
) {
    strip_empty_spans(dom);
    collapse_redundant_divs(dom, element_classes, element_styles, element_layout_hints);

    // First pass: compute (has_block_desc, has_text_desc) per node.
    let n = dom.len();
    let mut has_block_desc = vec![false; n];
    let mut has_text_desc = vec![false; n];
    // Reverse-post-order (children before parents): do iteratively.
    let mut order: Vec<NodeId> = Vec::with_capacity(n);
    let mut stack: Vec<NodeId> = vec![dom.root];
    while let Some(id) = stack.pop() {
        order.push(id);
        for &child in &dom.get(id).children {
            stack.push(child);
        }
    }
    // Process in reverse so children fold into parents.
    for id in order.iter().rev() {
        let elem = dom.get(*id);
        let mut block = has_block_desc[*id];
        let mut text = has_text_desc[*id];
        // Element's own text counts as text.
        if let Some(t) = &elem.text
            && t.chars().any(|c| !c.is_whitespace())
        {
            text = true;
        }
        for &child in &elem.children {
            let child_tag = dom.get(child).tag.clone();
            if is_block_tag(&child_tag) {
                block = true;
            }
            if has_block_desc[child] {
                block = true;
            }
            if has_text_desc[child] {
                text = true;
            }
            // Tail text on the child counts as text under this parent.
            if let Some(tail) = &dom.get(child).tail
                && tail.chars().any(|c| !c.is_whitespace())
            {
                text = true;
            }
        }
        has_block_desc[*id] = block;
        has_text_desc[*id] = text;
    }
    // Second pass: rename `<div>` to `<p>` when it's a leaf-text container.
    for id in 0..n {
        let elem = dom.get_mut(id);
        if elem.tag == "div" && !has_block_desc[id] && has_text_desc[id] {
            elem.tag = "p".to_string();
        }
    }

    // Third pass: heading promotion off the pending layout hints.
    let layout_hints: Vec<(NodeId, Vec<String>, Option<String>)> = element_layout_hints
        .iter()
        .map(|(k, (hints, level))| (*k, hints.clone(), level.clone()))
        .collect();
    for (id, hints, level) in layout_hints {
        if !hints.iter().any(|h| h == "heading") {
            continue;
        }
        if has_block_desc[id] {
            // Calibre's promotion requires `not contains_block_elem` —
            // a heading with block children would be invalid HTML.
            continue;
        }
        let elem = dom.get_mut(id);
        if elem.tag != "div" && elem.tag != "p" {
            continue;
        }
        let lvl = level.as_deref().unwrap_or("1");
        elem.tag = format!("h{}", lvl);
    }
}

/// Ensure every `<ol>`/`<ul>` has only `<li>` (plus `<script>`/`<template>`)
/// direct children — the only children a list may have (epubcheck RSC-005). KFX
/// lists sometimes carry trailing content (images, paragraphs) after the last
/// item as direct children of the list; absorb each stray child into the
/// preceding `<li>` (or a fresh one if the list opens with non-`<li>` content)
/// so the list is valid without dropping content.
pub fn normalize_lists_dom(dom: &mut Dom) {
    for id in 0..dom.len() {
        if !matches!(dom.get(id).tag.as_str(), "ol" | "ul") {
            continue;
        }
        let children = dom.get(id).children.clone();
        let allowed = |t: &str| matches!(t, "li" | "script" | "template");
        if children.iter().all(|&c| allowed(&dom.get(c).tag)) {
            continue;
        }
        let mut new_children: Vec<NodeId> = Vec::new();
        let mut current_li: Option<NodeId> = None;
        for c in children {
            if allowed(&dom.get(c).tag) {
                current_li = if dom.get(c).tag == "li" {
                    Some(c)
                } else {
                    None
                };
                new_children.push(c);
            } else {
                let li = match current_li {
                    Some(li) => li,
                    None => {
                        let li = dom.create_element("li");
                        dom.get_mut(li).parent = Some(id);
                        new_children.push(li);
                        current_li = Some(li);
                        li
                    }
                };
                dom.get_mut(c).parent = Some(li);
                dom.get_mut(li).children.push(c);
            }
        }
        dom.get_mut(id).children = new_children;
    }
}

/// Replace EOL characters (`\n` / `\r` / ` ` / ` `) inside
/// element text or tail with explicit `<br/>` elements. Mirrors calibre's
/// `replace_eol_with_br` (yj_to_epub_content.py:1720). KFX text content
/// carries forced line breaks as raw EOL characters; without this pass
/// they get collapsed by HTML whitespace rules and the source `<br/>`s
/// disappear from the rendered output.
///
/// One linear pass. After each split, the remainder of the source text
/// (which may still contain EOLs) rides on the new `<br/>`'s tail. The
/// new node is appended at the end of the arena — its id is past the cursor,
/// so the same walk visits it later. NodeIds are stable (the underlying
/// `Vec<Element>` is push-only), so insertion never shifts existing ids and
/// no restart is needed.
pub fn replace_eol_with_br_dom(dom: &mut Dom) {
    const EOL_CHARS: &[char] = &['\n', '\r', '\u{2028}', '\u{2029}'];
    let mut id = 0;
    while id < dom.len() {
        // Element text — split at the first EOL, insert `<br/>` as the
        // new first child. The new br's id is the highest in the arena,
        // so the same walk visits it later and handles its tail.
        if let Some(text) = dom.get(id).text.clone()
            && let Some(idx) = text.find(EOL_CHARS)
        {
            let eol_len = text[idx..].chars().next().unwrap().len_utf8();
            let head = text[..idx].to_string();
            let tail = text[idx + eol_len..].to_string();
            let br = dom.create_element("br");
            dom.get_mut(br).tail = if tail.is_empty() { None } else { Some(tail) };
            dom.get_mut(id).text = if head.is_empty() { None } else { Some(head) };
            dom.insert(id, 0, br);
        }
        // Element tail — split, insert `<br/>` as the next sibling.
        if let Some(tail_text) = dom.get(id).tail.clone()
            && let Some(idx) = tail_text.find(EOL_CHARS)
            && let Some(parent) = dom.get(id).parent
            && let Some(pos) = dom.child_index(parent, id)
        {
            let eol_len = tail_text[idx..].chars().next().unwrap().len_utf8();
            let head = tail_text[..idx].to_string();
            let tail = tail_text[idx + eol_len..].to_string();
            let br = dom.create_element("br");
            dom.get_mut(br).tail = if tail.is_empty() { None } else { Some(tail) };
            dom.get_mut(id).tail = if head.is_empty() { None } else { Some(head) };
            dom.insert(parent, pos + 1, br);
        }
        id += 1;
    }
}

/// Fold pending per-element classes + inline styles onto the DOM as actual
/// `class=` / `style=` attributes. Runs last, so both land after any
/// walk-time attributes (`src` / `href` / `id` / …) in insertion order —
/// the class-then-style tail both routes ship.
pub fn finalize_attrs(
    dom: &mut Dom,
    element_classes: &HashMap<NodeId, Vec<String>>,
    element_styles: &HashMap<NodeId, CssDecl>,
) {
    for (id, classes) in element_classes {
        if !classes.is_empty() {
            let joined = classes.join(" ");
            dom.get_mut(*id).set("class", joined);
        }
    }
    for (id, decl) in element_styles {
        if !decl.is_empty() {
            dom.get_mut(*id).set("style", decl.to_inline());
        }
    }
}
