//! Minimal mutable XHTML DOM for the kfx_to_epub content pipeline.
//!
//! Port of just enough lxml.etree to host the calibre content.py logic.
//! We use index-based parent/child relationships so methods can take a
//! `&mut Dom` and mutate any node without GAT acrobatics.
//!
//! Nodes have either text content (mixed text + children, lxml-style) or
//! a tag with attributes. We keep insertion order; `style` and `class`
//! are stored as regular attributes for simplicity.

#![allow(dead_code)]

use std::collections::HashMap;

pub type NodeId = usize;

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
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
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

    /// Convenience: create + append.
    pub fn sub_element(&mut self, parent: NodeId, tag: impl Into<String>) -> NodeId {
        let n = self.create_element(tag);
        self.append(parent, n);
        n
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
        if let Some(parent) = self.nodes[old].parent {
            if let Some(idx) = self.child_index(parent, old) {
                self.nodes[parent].children[idx] = new;
                self.nodes[new].parent = Some(parent);
                self.nodes[old].parent = None;
            }
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
}

fn is_void_element(tag: &str) -> bool {
    matches!(
        tag,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img"
        | "input" | "link" | "meta" | "source" | "track" | "wbr"
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
