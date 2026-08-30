//! A namespace-aware XML document tree.

use std::collections::HashMap;

use quick_xml::Reader;
use quick_xml::events::Event;

/// The XML namespace, bound to the `xml` prefix in every document without
/// declaration.
pub const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// An interned namespace URI. `None` in a [`Name`] means "no namespace", which
/// is a distinct value from any URI — an unprefixed attribute has no namespace
/// even when its element has a default one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NsId(u32);

/// An index into [`Document`]'s node arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(u32);

impl NodeId {
    /// The arena slot this id names, for a caller keeping one value per node
    /// (a set membership, a mark) in a flat vector of [`Document::len`].
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// An expanded name: a namespace (or none) plus a local name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Name {
    pub ns: Option<NsId>,
    pub local: String,
}

/// One attribute of an element, in document order.
#[derive(Debug, Clone)]
pub struct Attr {
    pub name: Name,
    pub value: String,
}

/// An element's own data. Children live in [`Node::children`].
#[derive(Debug, Clone)]
pub struct Element {
    pub name: Name,
    pub attrs: Vec<Attr>,
    /// The prefix as written, kept only for diagnostics — nothing matches on it.
    pub prefix: Option<String>,
    /// The `xmlns`/`xmlns:p` declarations written on this element, as
    /// `(prefix, uri)` with an empty prefix for the default namespace.
    pub namespaces: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub enum NodeKind {
    /// The document node; its single element child is the root element.
    Document,
    Element(Element),
    /// A merged run of character data (text and CDATA), entity-expanded.
    Text(String),
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    /// Byte offset of the node's start in the source, so a finding about it can
    /// name a line. See [`Document::line`].
    pub offset: usize,
}

/// A parsed XML document.
#[derive(Debug, Clone)]
pub struct Document {
    nodes: Vec<Node>,
    /// Interned namespace URIs; [`NsId`] indexes this.
    namespaces: Vec<String>,
    /// Byte offset of each line's first character, so [`Document::line`] is a
    /// binary search rather than a scan of the whole source per finding.
    line_starts: Vec<usize>,
    /// Every node in document order — the answer to `//`, which an assertion
    /// may ask once per node it fires on. Filled on first use.
    all: std::cell::OnceCell<Vec<NodeId>>,
}

impl Document {
    /// Parse `xml` into a tree, or report the first well-formedness error.
    pub fn parse(xml: &str) -> Result<Document, ParseError> {
        Parser::new().parse(xml)
    }

    /// The document node, whose element child is the root element.
    pub fn root(&self) -> NodeId {
        NodeId(0)
    }

    /// The root element, or `None` for a document with no element (which
    /// [`parse`](Self::parse) rejects, so this is `Some` for any parsed tree).
    pub fn root_element(&self) -> Option<NodeId> {
        self.element_children(self.root()).next()
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0 as usize]
    }

    /// The element data of `id`, or `None` if it is not an element.
    pub fn element(&self, id: NodeId) -> Option<&Element> {
        match &self.node(id).kind {
            NodeKind::Element(e) => Some(e),
            _ => None,
        }
    }

    /// The element data of `id` for modification — what [`super::preprocess`]
    /// needs to reproduce the transformations epubcheck applies to a document
    /// before its schemas ever see it.
    pub fn element_mut(&mut self, id: NodeId) -> Option<&mut Element> {
        match &mut self.nodes[id.0 as usize].kind {
            NodeKind::Element(e) => Some(e),
            _ => None,
        }
    }

    /// Intern a namespace URI, adding it if the document does not use it yet.
    pub fn intern_namespace(&mut self, uri: &str) -> NsId {
        if let Some(id) = self.ns_id(uri) {
            return id;
        }
        self.namespaces.push(uri.to_string());
        NsId(self.namespaces.len() as u32 - 1)
    }

    /// The namespace URI behind an [`NsId`].
    pub fn namespace(&self, ns: NsId) -> &str {
        &self.namespaces[ns.0 as usize]
    }

    /// `(namespace-uri, local-name)` of a name, for matching against a schema.
    pub fn expanded<'a>(&'a self, name: &'a Name) -> (Option<&'a str>, &'a str) {
        (name.ns.map(|n| self.namespace(n)), name.local.as_str())
    }

    /// The `NsId` an already-interned URI has, or `None` when the document does
    /// not use that namespace at all — in which case no name in it can match.
    pub fn ns_id(&self, uri: &str) -> Option<NsId> {
        self.namespaces
            .iter()
            .position(|u| u == uri)
            .map(|i| NsId(i as u32))
    }

    pub fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.node(id).children.iter().copied()
    }

    /// The element children of `id`, in document order.
    pub fn element_children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.children(id)
            .filter(|c| matches!(self.node(*c).kind, NodeKind::Element(_)))
    }

    /// Every node of the subtree rooted at `id`, in document order, `id` first.
    pub fn descendants(&self, id: NodeId) -> Vec<NodeId> {
        if id == self.root() {
            return self.all_nodes().to_vec();
        }
        let mut out = Vec::new();
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            out.push(n);
            stack.extend(self.node(n).children.iter().rev().copied());
        }
        out
    }

    /// Every node in document order, walked once per document.
    pub fn all_nodes(&self) -> &[NodeId] {
        self.all.get_or_init(|| {
            let mut out = Vec::with_capacity(self.nodes.len());
            let mut stack = vec![self.root()];
            while let Some(n) = stack.pop() {
                out.push(n);
                stack.extend(self.node(n).children.iter().rev().copied());
            }
            out
        })
    }

    /// The concatenated text of the subtree rooted at `id` — XPath's
    /// `string-value`, which Schematron assertions are written against.
    pub fn string_value(&self, id: NodeId) -> String {
        let mut out = String::new();
        for n in self.descendants(id) {
            if let NodeKind::Text(t) = &self.node(n).kind {
                out.push_str(t);
            }
        }
        out
    }

    /// An attribute value by expanded name. `ns` is a URI, or `None` for the
    /// no-namespace attributes that make up almost every grammar's attlist.
    pub fn attr(&self, id: NodeId, ns: Option<&str>, local: &str) -> Option<&str> {
        let element = self.element(id)?;
        element
            .attrs
            .iter()
            .find(|a| a.name.local == local && self.expanded(&a.name).0 == ns)
            .map(|a| a.value.as_str())
    }

    /// The namespace URI a prefix is bound to at `id`, per the declarations on
    /// it and its ancestors — the innermost binding wins.
    pub fn in_scope_namespace(&self, id: NodeId, prefix: &str) -> Option<&str> {
        if prefix == "xml" {
            return Some("http://www.w3.org/XML/1998/namespace");
        }
        let mut node = Some(id);
        while let Some(current) = node {
            if let Some(element) = self.element(current)
                && let Some((_, uri)) = element.namespaces.iter().find(|(p, _)| p == prefix)
            {
                return Some(uri);
            }
            node = self.node(current).parent;
        }
        None
    }

    /// This document's line map, to hand to a [`Builder`] so a section built
    /// from it reports lines in the original source rather than in itself.
    pub fn line_map(&self) -> Vec<usize> {
        self.line_starts.clone()
    }

    /// The 1-based source line a node starts on.
    pub fn line(&self, id: NodeId) -> u32 {
        let offset = self.node(id).offset;
        match self.line_starts.binary_search(&offset) {
            Ok(i) => i as u32 + 1,
            Err(i) => i as u32,
        }
    }

    /// How many nodes the tree holds, for sizing and tests.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }
}

/// Builds a [`Document`] node by node.
pub struct Builder {
    nodes: Vec<Node>,
    namespaces: Vec<String>,
    intern: HashMap<String, NsId>,
    line_starts: Vec<usize>,
}

impl Builder {
    /// Start an empty document that reports lines against `source`'s line map —
    /// see [`Document::line_map`].
    pub fn new(line_starts: Vec<usize>) -> Self {
        Builder {
            nodes: vec![Node {
                kind: NodeKind::Document,
                parent: None,
                children: Vec::new(),
                offset: 0,
            }],
            namespaces: Vec::new(),
            intern: HashMap::new(),
            line_starts,
        }
    }

    /// The document node, the parent of the root element to come.
    pub fn root(&self) -> NodeId {
        NodeId(0)
    }

    fn intern_ns(&mut self, uri: &str) -> NsId {
        if let Some(id) = self.intern.get(uri) {
            return *id;
        }
        let id = NsId(self.namespaces.len() as u32);
        self.namespaces.push(uri.to_string());
        self.intern.insert(uri.to_string(), id);
        id
    }

    fn name(&mut self, ns: Option<&str>, local: &str) -> Name {
        Name {
            ns: ns.map(|u| self.intern_ns(u)),
            local: local.to_string(),
        }
    }

    /// Append an element, with its attributes as `(namespace, local, value)`.
    pub fn push_element(
        &mut self,
        parent: NodeId,
        ns: Option<&str>,
        local: &str,
        attrs: &[(Option<String>, String, String)],
        prefix: Option<String>,
        offset: usize,
    ) -> NodeId {
        let name = self.name(ns, local);
        let attrs = attrs
            .iter()
            .map(|(ns, local, value)| Attr {
                name: self.name(ns.as_deref(), local),
                value: value.clone(),
            })
            .collect();
        self.push(
            NodeKind::Element(Element {
                name,
                attrs,
                prefix,
                namespaces: Vec::new(),
            }),
            parent,
            offset,
        )
    }

    pub fn push_text(&mut self, parent: NodeId, text: String, offset: usize) -> NodeId {
        self.push(NodeKind::Text(text), parent, offset)
    }

    fn push(&mut self, kind: NodeKind, parent: NodeId, offset: usize) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node {
            kind,
            parent: Some(parent),
            children: Vec::new(),
            offset,
        });
        self.nodes[parent.0 as usize].children.push(id);
        id
    }

    pub fn finish(self) -> Document {
        Document {
            nodes: self.nodes,
            namespaces: self.namespaces,
            line_starts: self.line_starts,
            all: std::cell::OnceCell::new(),
        }
    }
}

/// A well-formedness or namespace error, with the position where it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    /// Byte offset into the source, for a diagnostic that can be located.
    pub offset: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (at byte {})", self.message, self.offset)
    }
}

/// The in-progress parse: the arena, the interning table, and the namespace
/// scope stack.
struct Parser {
    nodes: Vec<Node>,
    namespaces: Vec<String>,
    intern: HashMap<String, NsId>,
    /// One frame per open element: the prefix→URI bindings it declared. A lookup
    /// walks the stack from the top, which is how XML scoping is defined.
    scopes: Vec<Vec<(String, String)>>,
}

impl Parser {
    fn new() -> Self {
        Parser {
            nodes: vec![Node {
                kind: NodeKind::Document,
                parent: None,
                children: Vec::new(),
                offset: 0,
            }],
            namespaces: Vec::new(),
            intern: HashMap::new(),
            scopes: Vec::new(),
        }
    }

    fn intern_ns(&mut self, uri: &str) -> NsId {
        if let Some(id) = self.intern.get(uri) {
            return *id;
        }
        let id = NsId(self.namespaces.len() as u32);
        self.namespaces.push(uri.to_string());
        self.intern.insert(uri.to_string(), id);
        id
    }

    /// Resolve a prefix through the scope stack. The empty prefix is the default
    /// namespace; `xml` is always bound; an unbound prefix yields `None`.
    fn resolve(&self, prefix: &str) -> Option<String> {
        if prefix == "xml" {
            return Some(XML_NS.to_string());
        }
        for frame in self.scopes.iter().rev() {
            for (p, uri) in frame {
                if p == prefix {
                    return (!uri.is_empty()).then(|| uri.clone());
                }
            }
        }
        None
    }

    fn parse(mut self, xml: &str) -> Result<Document, ParseError> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(false);
        reader.config_mut().check_end_names = true;
        // Character data is accumulated and flushed as one text node, so that
        // `<p>a<![CDATA[b]]>c</p>` has a single child, as the data model says.
        let mut pending = String::new();
        let mut pending_at = 0usize;
        let mut open: Vec<NodeId> = vec![NodeId(0)];

        loop {
            let offset = reader.buffer_position() as usize;
            match reader.read_event() {
                Ok(Event::Start(e)) => {
                    self.flush_text(&mut pending, *open.last().expect("root frame"), pending_at);
                    let id = self.open_element(&e, *open.last().unwrap(), offset)?;
                    open.push(id);
                }
                Ok(Event::Empty(e)) => {
                    self.flush_text(&mut pending, *open.last().expect("root frame"), pending_at);
                    self.open_element(&e, *open.last().unwrap(), offset)?;
                    self.scopes.pop();
                }
                Ok(Event::End(_)) => {
                    self.flush_text(&mut pending, *open.last().expect("root frame"), pending_at);
                    self.scopes.pop();
                    open.pop();
                    if open.is_empty() {
                        return Err(ParseError {
                            message: "end tag with no matching start tag".into(),
                            offset,
                        });
                    }
                }
                Ok(Event::Text(t)) => {
                    // Entity references expand here, so the tree holds character
                    // data — which is what a datatype check or a Schematron
                    // string comparison is written against.
                    let text = t.xml_content().map_err(|e| ParseError {
                        message: e.to_string(),
                        offset,
                    })?;
                    if pending.is_empty() {
                        pending_at = offset;
                    }
                    pending.push_str(&text);
                }
                Ok(Event::CData(c)) => {
                    // CDATA is character data verbatim — no entity expansion.
                    if pending.is_empty() {
                        pending_at = offset;
                    }
                    pending.push_str(&String::from_utf8_lossy(c.as_ref()));
                }
                Ok(Event::GeneralRef(r)) => {
                    // quick-xml surfaces every `&name;` as its own event, so a
                    // text run containing one arrives split around it.
                    if pending.is_empty() {
                        pending_at = offset;
                    }
                    let name = String::from_utf8_lossy(r.as_ref()).into_owned();
                    pending.push_str(&expand_entity(&name));
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    return Err(ParseError {
                        message: e.to_string(),
                        offset,
                    });
                }
                // Comments, PIs, the XML declaration and the DOCTYPE are not in
                // the data model the schemas validate against.
                _ => {}
            }
        }
        if open.len() != 1 {
            return Err(ParseError {
                message: "element left open at end of document".into(),
                offset: xml.len(),
            });
        }
        let root = Document {
            nodes: self.nodes,
            namespaces: self.namespaces,
            line_starts: std::iter::once(0)
                .chain(xml.match_indices('\n').map(|(i, _)| i + 1))
                .collect(),
            all: std::cell::OnceCell::new(),
        };
        if root.root_element().is_none() {
            return Err(ParseError {
                message: "document has no root element".into(),
                offset: xml.len(),
            });
        }
        Ok(root)
    }

    /// Append the accumulated character data, if any, as one text node.
    fn flush_text(&mut self, pending: &mut String, parent: NodeId, offset: usize) {
        if pending.is_empty() {
            return;
        }
        let text = std::mem::take(pending);
        // Character data before the root element is not part of any element's
        // content; the well-formedness pass reports it, the tree drops it.
        if parent == NodeId(0) {
            return;
        }
        self.push(NodeKind::Text(text), parent, offset);
    }

    fn push(&mut self, kind: NodeKind, parent: NodeId, offset: usize) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node {
            kind,
            parent: Some(parent),
            children: Vec::new(),
            offset,
        });
        self.nodes[parent.0 as usize].children.push(id);
        id
    }

    /// Push a namespace scope for `e`, resolve its name and attributes, and add
    /// the element node. The caller pops the scope (at `End`, or immediately for
    /// an empty element).
    fn open_element(
        &mut self,
        e: &quick_xml::events::BytesStart,
        parent: NodeId,
        offset: usize,
    ) -> Result<NodeId, ParseError> {
        // Declarations first: an element's own xmlns applies to its own name.
        let mut frame = Vec::new();
        for attr in e.attributes() {
            let attr = attr.map_err(|err| ParseError {
                message: err.to_string(),
                offset,
            })?;
            let key = attr.key.as_ref();
            let value = || String::from_utf8_lossy(&attr.value).into_owned();
            if key == b"xmlns" {
                frame.push((String::new(), value()));
            } else if let Some(prefix) = key.strip_prefix(b"xmlns:") {
                frame.push((String::from_utf8_lossy(prefix).into_owned(), value()));
            }
        }
        self.scopes.push(frame);

        // Exactly one element may be the child of the document node; a second is
        // content after the root, which is not well-formed.
        if parent == NodeId(0)
            && self.nodes[0]
                .children
                .iter()
                .any(|c| matches!(self.nodes[c.0 as usize].kind, NodeKind::Element(_)))
        {
            return Err(ParseError {
                message: "content after the root element".into(),
                offset,
            });
        }

        let qname = e.name();
        let (prefix, local) = split_qname(qname.as_ref());
        let prefix = String::from_utf8_lossy(prefix).into_owned();
        let local = String::from_utf8_lossy(local).into_owned();
        let ns = match self.resolve(&prefix) {
            Some(uri) => Some(self.intern_ns(&uri)),
            None if prefix.is_empty() => None, // no default namespace in scope
            None => {
                return Err(ParseError {
                    message: format!("unbound namespace prefix {prefix:?} on element {local:?}"),
                    offset,
                });
            }
        };

        let mut attrs = Vec::new();
        let mut namespaces = Vec::new();
        for attr in e.attributes() {
            let attr = attr.map_err(|err| ParseError {
                message: err.to_string(),
                offset,
            })?;
            let key = attr.key.as_ref();
            // A declaration is not an attribute of the data model, but it is
            // kept: a schema document quotes prefixed names in its own attribute
            // values, and they resolve against these.
            if key == b"xmlns" || key.starts_with(b"xmlns:") {
                let prefix = String::from_utf8_lossy(&key[b"xmlns".len()..]);
                namespaces.push((
                    prefix.trim_start_matches(':').to_string(),
                    String::from_utf8_lossy(&attr.value).into_owned(),
                ));
                continue;
            }
            let (aprefix, alocal) = split_qname(key);
            let aprefix = String::from_utf8_lossy(aprefix).into_owned();
            let alocal = String::from_utf8_lossy(alocal).into_owned();
            // An unprefixed attribute is in NO namespace — the element's default
            // namespace never applies to it.
            let ans = if aprefix.is_empty() {
                None
            } else {
                match self.resolve(&aprefix) {
                    Some(uri) => Some(self.intern_ns(&uri)),
                    None => {
                        return Err(ParseError {
                            message: format!(
                                "unbound namespace prefix {aprefix:?} on attribute {alocal:?}"
                            ),
                            offset,
                        });
                    }
                }
            };
            let name = Name {
                ns: ans,
                local: alocal,
            };
            if attrs.iter().any(|a: &Attr| a.name == name) {
                return Err(ParseError {
                    message: format!("duplicate attribute {:?}", name.local),
                    offset,
                });
            }
            // The source is already `str`, so the bytes are UTF-8; only entity
            // expansion is left to do, under the same rules as text content.
            let raw = String::from_utf8_lossy(&attr.value);
            let value = expand_entities_in(&raw);
            attrs.push(Attr { name, value });
        }

        Ok(self.push(
            NodeKind::Element(Element {
                name: Name { ns, local },
                attrs,
                prefix: (!prefix.is_empty()).then_some(prefix),
                namespaces,
            }),
            parent,
            offset,
        ))
    }
}

/// Resolve one entity reference (the text between `&` and `;`).
fn expand_entity(name: &str) -> String {
    match name {
        "amp" => return "&".to_string(),
        "lt" => return "<".to_string(),
        "gt" => return ">".to_string(),
        "quot" => return "\"".to_string(),
        "apos" => return "'".to_string(),
        _ => {}
    }
    if let Some(digits) = name.strip_prefix('#') {
        let code = match digits.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => digits.parse::<u32>().ok(),
        };
        if let Some(c) = code.and_then(char::from_u32) {
            return c.to_string();
        }
    }
    format!("&{name};")
}

/// Expand every `&…;` in a string, for the contexts quick-xml hands over raw
/// (attribute values). Text content arrives pre-split into `GeneralRef` events
/// and goes through [`expand_entity`] directly.
pub(crate) fn expand_entities_in(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        match after.find(';') {
            // A `&` with no `;` is not a reference; keep it verbatim.
            None => {
                out.push_str(&rest[amp..]);
                return out;
            }
            Some(end) => {
                out.push_str(&expand_entity(&after[..end]));
                rest = &after[end + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Split a qualified name into `(prefix, local)`; the prefix is empty when the
/// name is unprefixed.
fn split_qname(name: &[u8]) -> (&[u8], &[u8]) {
    match name.iter().position(|b| *b == b':') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (b"", name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_namespaces_to_uris_not_prefixes() {
        let doc = Document::parse(
            r#"<h:html xmlns:h="http://www.w3.org/1999/xhtml" xmlns:e="http://www.idpf.org/2007/ops">
                 <h:body e:type="bodymatter" id="b"><h:p>hi</h:p></h:body>
               </h:html>"#,
        )
        .unwrap();
        let root = doc.root_element().unwrap();
        assert_eq!(
            doc.expanded(&doc.element(root).unwrap().name),
            (Some("http://www.w3.org/1999/xhtml"), "html")
        );
        let body = doc.element_children(root).next().unwrap();
        // The same namespace under a different prefix is the same NsId.
        assert_eq!(
            doc.element(body).unwrap().name.ns,
            doc.element(root).unwrap().name.ns
        );
        // A prefixed attribute takes its prefix's namespace…
        assert_eq!(
            doc.attr(body, Some("http://www.idpf.org/2007/ops"), "type"),
            Some("bodymatter")
        );
        // …and an unprefixed one is in no namespace, never the element's default.
        assert_eq!(doc.attr(body, None, "id"), Some("b"));
        assert_eq!(
            doc.attr(body, Some("http://www.w3.org/1999/xhtml"), "id"),
            None
        );
    }

    #[test]
    fn default_namespace_applies_to_elements_only() {
        let doc = Document::parse(
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><body class="c"/></html>"#,
        )
        .unwrap();
        let body = doc
            .element_children(doc.root_element().unwrap())
            .next()
            .unwrap();
        assert_eq!(
            doc.expanded(&doc.element(body).unwrap().name).0,
            Some("http://www.w3.org/1999/xhtml")
        );
        assert_eq!(doc.attr(body, None, "class"), Some("c"));
    }

    #[test]
    fn xml_prefix_is_bound_without_declaration() {
        let doc = Document::parse(r#"<a xml:lang="ja" xml:space="preserve"/>"#).unwrap();
        let root = doc.root_element().unwrap();
        assert_eq!(doc.attr(root, Some(XML_NS), "lang"), Some("ja"));
        assert_eq!(doc.attr(root, Some(XML_NS), "space"), Some("preserve"));
    }

    #[test]
    fn namespace_declarations_are_not_attributes() {
        let doc = Document::parse(r#"<a xmlns="urn:x" xmlns:p="urn:y" real="1"/>"#).unwrap();
        let root = doc.root_element().unwrap();
        let attrs = &doc.element(root).unwrap().attrs;
        assert_eq!(attrs.len(), 1, "only `real` is an attribute: {attrs:?}");
        assert_eq!(attrs[0].name.local, "real");
    }

    #[test]
    fn scopes_pop_with_their_element() {
        // `p` is bound only inside <b>; using it after must not resolve.
        let doc = Document::parse(r#"<a xmlns:q="urn:q"><b xmlns:p="urn:p"><p:c/></b><q:d/></a>"#)
            .unwrap();
        assert!(doc.ns_id("urn:p").is_some());
        let err = Document::parse(r#"<a><b xmlns:p="urn:p"/><p:c/></a>"#).unwrap_err();
        assert!(err.message.contains("unbound namespace prefix"), "{err}");
    }

    #[test]
    fn text_and_cdata_merge_into_one_node() {
        let doc = Document::parse(r#"<p>a<![CDATA[<b>]]>c &amp; d</p>"#).unwrap();
        let root = doc.root_element().unwrap();
        assert_eq!(doc.node(root).children.len(), 1, "one merged text run");
        assert_eq!(doc.string_value(root), "a<b>c & d");
    }

    #[test]
    fn rejects_documents_a_schema_could_not_validate() {
        for (xml, expect) in [
            ("<a><b></a>", "expected `</b>`"),
            ("<a>", "left open"),
            ("<a/><b/>", ""), // content after the root element
            ("<a x='1' x='2'/>", "duplicated attribute"),
            ("<p:a/>", "unbound namespace prefix"),
            ("", "no root element"),
        ] {
            let err = Document::parse(xml).unwrap_err();
            assert!(
                expect.is_empty() || err.message.to_lowercase().contains(expect),
                "{xml:?}: expected {expect:?}, got {err}"
            );
        }
    }

    #[test]
    fn walks_the_tree_in_document_order() {
        let doc = Document::parse("<a><b/><c><d/></c></a>").unwrap();
        let root = doc.root_element().unwrap();
        let names: Vec<&str> = doc
            .descendants(root)
            .iter()
            .filter_map(|n| doc.element(*n))
            .map(|e| e.name.local.as_str())
            .collect();
        assert_eq!(names, ["a", "b", "c", "d"]);
    }
}
