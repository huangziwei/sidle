//! Validation by pattern derivative — James Clark's *An algorithm for RELAX NG
//! validation*, transcribed onto the interned model of [`super::pattern`].

use std::collections::HashMap;

use super::datatype;
use super::pattern::{Arena, Pattern, PatternId};
use crate::validate::source::epub::xml::tree::{Document, NodeId, NodeKind};

/// The continuation `applyAfter` carries — the paper's `\p -> …` closures, of
/// which the algorithm builds exactly these four shapes.
#[derive(Debug, Clone, Copy)]
enum Cont {
    /// `\p -> group p q`
    GroupRight(PatternId),
    /// `\p -> interleave p q`
    InterleaveRight(PatternId),
    /// `\p -> interleave q p`
    InterleaveLeft(PatternId),
    /// `\p -> after p q`
    AfterRight(PatternId),
}

/// Where validation failed, in enough detail to name the offending construct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The node the derivative rejected.
    pub node: NodeId,
    pub message: String,
}

/// A grammar plus the caches one validation run fills.
pub struct Validator<'a> {
    arena: &'a mut Arena,
    /// `start_tag_open_deriv` keyed by pattern and element name.
    open_cache: HashMap<(PatternId, Option<String>, String), PatternId>,
    /// `start_tag_close_deriv` and `end_tag_deriv`, keyed by pattern.
    close_cache: HashMap<PatternId, PatternId>,
    end_cache: HashMap<PatternId, PatternId>,
    /// The first failure seen; validation continues so the whole document is
    /// walked, but only the first violation is reported per element subtree.
    violations: Vec<Violation>,
}

impl<'a> Validator<'a> {
    pub fn new(arena: &'a mut Arena) -> Self {
        Validator {
            arena,
            open_cache: HashMap::new(),
            close_cache: HashMap::new(),
            end_cache: HashMap::new(),
            violations: Vec::new(),
        }
    }

    /// Validate `doc`'s root element against `start`, returning every violation
    /// found. An empty result means the document matches the grammar.
    pub fn validate(&mut self, doc: &Document, start: PatternId) -> Vec<Violation> {
        self.violations.clear();
        let Some(root) = doc.root_element() else {
            return vec![Violation {
                node: doc.root(),
                message: "document has no root element".into(),
            }];
        };
        let end = self.child_deriv(doc, start, root);
        if self.arena.is(end, &Pattern::NotAllowed) && self.violations.is_empty() {
            let name = &doc.element(root).expect("root is an element").name.local;
            self.violations.push(Violation {
                node: root,
                message: format!("element {name:?} is not allowed as the root of this document"),
            });
        }
        std::mem::take(&mut self.violations)
    }

    /// The derivative of `p` after one child node — the paper's `childDeriv`.
    fn child_deriv(&mut self, doc: &Document, p: PatternId, node: NodeId) -> PatternId {
        match &doc.node(node).kind {
            NodeKind::Text(s) => {
                let s = s.clone();
                self.text_deriv(p, &s)
            }
            NodeKind::Element(element) => {
                let (ns, local) = doc.expanded(&element.name);
                let (ns, local) = (ns.map(str::to_string), local.to_string());

                let after_open = self.start_tag_open_deriv(p, ns.as_deref(), &local);
                if self.arena.is(after_open, &Pattern::NotAllowed) {
                    self.fail(
                        node,
                        format!("element {local:?} is not allowed here"),
                        Some(p),
                    );
                    return after_open;
                }

                let mut current = after_open;
                for attr in &element.attrs {
                    let (ans, alocal) = doc.expanded(&attr.name);
                    let next = self.att_deriv(current, ans, alocal, &attr.value);
                    if self.arena.is(next, &Pattern::NotAllowed) {
                        // An attribute the grammar declares here but whose value
                        let by_name = self.att_name_deriv(current, ans, alocal);
                        match self.arena.is(by_name, &Pattern::NotAllowed) {
                            true => self.fail(
                                node,
                                format!("attribute {alocal:?} is not allowed on element {local:?}"),
                                Some(current),
                            ),
                            false => self.fail(
                                node,
                                format!(
                                    "value of attribute {alocal:?} is invalid: {}",
                                    self.attribute_datatype(current, ans, alocal, &attr.value)
                                ),
                                None,
                            ),
                        }
                        return next;
                    }
                    current = next;
                }

                let closed = self.start_tag_close_deriv(current);
                if self.arena.is(closed, &Pattern::NotAllowed) {
                    self.fail(
                        node,
                        format!("element {local:?} is missing a required attribute"),
                        Some(current),
                    );
                    return closed;
                }

                let after_children = self.children_deriv(doc, closed, node);
                if self.arena.is(after_children, &Pattern::NotAllowed) {
                    return after_children;
                }
                let ended = self.end_tag_deriv(after_children);
                if self.arena.is(ended, &Pattern::NotAllowed) {
                    self.fail(
                        node,
                        format!("element {local:?} is incomplete — required content is missing"),
                        Some(after_children),
                    );
                }
                ended
            }
            NodeKind::Document => p,
        }
    }

    /// Record a violation, unless one is already pending for this subtree. Given
    fn fail(&mut self, node: NodeId, message: String, context: Option<PatternId>) {
        if !self.violations.is_empty() {
            return; // the first failure explains the rest
        }
        let mut expected = Vec::new();
        if let Some(context) = context {
            self.collect_expected(context, &mut expected);
        }
        expected.sort();
        expected.dedup();
        let message = if expected.is_empty() {
            message
        } else {
            let list = expected
                .iter()
                .map(|n| format!("{n:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{message}; expected {list}")
        };
        self.violations.push(Violation { node, message });
    }

    /// The element and attribute names a pattern could accept next, for the
    /// diagnostic. A wildcard contributes nothing, so the list under-claims.
    fn collect_expected(&self, p: PatternId, out: &mut Vec<String>) {
        if out.len() > 24 {
            return; // a long list stops informing
        }
        match self.arena.pattern(p).clone() {
            Pattern::Element(nc, _) | Pattern::Attribute(nc, _) => {
                self.arena.expected_names(nc, out)
            }
            Pattern::Choice(a, b) | Pattern::Group(a, b) | Pattern::Interleave(a, b) => {
                self.collect_expected(a, out);
                self.collect_expected(b, out);
            }
            Pattern::OneOrMore(a) | Pattern::After(a, _) => self.collect_expected(a, out),
            _ => {}
        }
    }

    /// The paper's `childrenDeriv`: derive over an element's children, with the
    /// rule that whitespace-only content may also be *no* content — which is why
    /// `<p>\n</p>` matches `element p { empty }`.
    fn children_deriv(&mut self, doc: &Document, p: PatternId, parent: NodeId) -> PatternId {
        let children: Vec<NodeId> = doc.children(parent).collect();
        let only_text = match children.as_slice() {
            [] => Some(String::new()),
            [one] => match &doc.node(*one).kind {
                NodeKind::Text(s) => Some(s.clone()),
                _ => None,
            },
            _ => None,
        };
        if let Some(text) = only_text {
            let derived = self.text_deriv(p, &text);
            if text.chars().all(char::is_whitespace) {
                // Either the text is consumed, or the element is treated as empty.
                return self.arena.choice(p, derived);
            }
            if self.arena.is(derived, &Pattern::NotAllowed) {
                let shown: String = text.chars().take(30).collect();
                self.fail(
                    parent,
                    format!("text {shown:?} is not allowed here"),
                    Some(p),
                );
            }
            return derived;
        }
        // Mixed content: whitespace-only text nodes between elements are
        // stripped, as the specification's `strip` requires.
        let mut current = p;
        for child in children {
            if let NodeKind::Text(s) = &doc.node(child).kind
                && s.chars().all(char::is_whitespace)
            {
                continue;
            }
            current = self.child_deriv(doc, current, child);
            if self.arena.is(current, &Pattern::NotAllowed) {
                if let NodeKind::Text(_) = &doc.node(child).kind {
                    self.fail(child, "text is not allowed here".into(), Some(p));
                }
                return current;
            }
        }
        current
    }

    /// The paper's `textDeriv`.
    fn text_deriv(&mut self, p: PatternId, s: &str) -> PatternId {
        match self.arena.pattern(p).clone() {
            Pattern::Choice(a, b) => {
                let (x, y) = (self.text_deriv(a, s), self.text_deriv(b, s));
                self.arena.choice(x, y)
            }
            Pattern::Interleave(a, b) => {
                let da = self.text_deriv(a, s);
                let left = self.arena.interleave(da, b);
                let db = self.text_deriv(b, s);
                let right = self.arena.interleave(a, db);
                self.arena.choice(left, right)
            }
            Pattern::Group(a, b) => {
                let da = self.text_deriv(a, s);
                let first = self.arena.group(da, b);
                if self.arena.nullable(a) {
                    let db = self.text_deriv(b, s);
                    self.arena.choice(first, db)
                } else {
                    first
                }
            }
            Pattern::After(a, b) => {
                let da = self.text_deriv(a, s);
                self.arena.after(da, b)
            }
            Pattern::OneOrMore(a) => {
                let da = self.text_deriv(a, s);
                let more = self.arena.zero_or_more(a);
                self.arena.group(da, more)
            }
            Pattern::Text => p,
            Pattern::Value { datatype, value } => {
                if datatype::equal(&datatype.library, &datatype.name, &value, s) {
                    self.arena.empty()
                } else {
                    self.arena.not_allowed()
                }
            }
            Pattern::Data { datatype, params } => {
                if datatype::allows(&datatype.library, &datatype.name, &params, s) {
                    self.arena.empty()
                } else {
                    self.arena.not_allowed()
                }
            }
            Pattern::DataExcept {
                datatype,
                params,
                except,
            } => {
                let allowed = datatype::allows(&datatype.library, &datatype.name, &params, s);
                let excluded = {
                    let d = self.text_deriv(except, s);
                    self.arena.nullable(d)
                };
                if allowed && !excluded {
                    self.arena.empty()
                } else {
                    self.arena.not_allowed()
                }
            }
            Pattern::List(inner) => {
                let mut current = inner;
                for token in s.split_whitespace() {
                    current = self.text_deriv(current, token);
                }
                if self.arena.nullable(current) {
                    self.arena.empty()
                } else {
                    self.arena.not_allowed()
                }
            }
            _ => self.arena.not_allowed(),
        }
    }

    /// The paper's `attDeriv`, for one attribute.
    fn att_deriv(&mut self, p: PatternId, ns: Option<&str>, local: &str, value: &str) -> PatternId {
        match self.arena.pattern(p).clone() {
            Pattern::After(a, b) => {
                let da = self.att_deriv(a, ns, local, value);
                self.arena.after(da, b)
            }
            Pattern::Choice(a, b) => {
                let x = self.att_deriv(a, ns, local, value);
                let y = self.att_deriv(b, ns, local, value);
                self.arena.choice(x, y)
            }
            Pattern::Group(a, b) => {
                let da = self.att_deriv(a, ns, local, value);
                let left = self.arena.group(da, b);
                let db = self.att_deriv(b, ns, local, value);
                let right = self.arena.group(a, db);
                self.arena.choice(left, right)
            }
            Pattern::Interleave(a, b) => {
                let da = self.att_deriv(a, ns, local, value);
                let left = self.arena.interleave(da, b);
                let db = self.att_deriv(b, ns, local, value);
                let right = self.arena.interleave(a, db);
                self.arena.choice(left, right)
            }
            Pattern::OneOrMore(a) => {
                let da = self.att_deriv(a, ns, local, value);
                let more = self.arena.zero_or_more(a);
                self.arena.group(da, more)
            }
            Pattern::Attribute(nc, content) => {
                if self.arena.name_matches(nc, ns, local) && self.value_match(content, value) {
                    self.arena.empty()
                } else {
                    self.arena.not_allowed()
                }
            }
            _ => self.arena.not_allowed(),
        }
    }

    /// `att_deriv` with the value check removed: would this attribute *name* be
    /// accepted here at all? An unknown name is the defect worth listing names for.
    fn att_name_deriv(&mut self, p: PatternId, ns: Option<&str>, local: &str) -> PatternId {
        match self.arena.pattern(p).clone() {
            Pattern::After(a, b) => {
                let da = self.att_name_deriv(a, ns, local);
                self.arena.after(da, b)
            }
            Pattern::Choice(a, b) => {
                let x = self.att_name_deriv(a, ns, local);
                let y = self.att_name_deriv(b, ns, local);
                self.arena.choice(x, y)
            }
            Pattern::Group(a, b) => {
                let da = self.att_name_deriv(a, ns, local);
                let left = self.arena.group(da, b);
                let db = self.att_name_deriv(b, ns, local);
                let right = self.arena.group(a, db);
                self.arena.choice(left, right)
            }
            Pattern::Interleave(a, b) => {
                let da = self.att_name_deriv(a, ns, local);
                let left = self.arena.interleave(da, b);
                let db = self.att_name_deriv(b, ns, local);
                let right = self.arena.interleave(a, db);
                self.arena.choice(left, right)
            }
            Pattern::OneOrMore(a) => {
                let da = self.att_name_deriv(a, ns, local);
                let more = self.arena.zero_or_more(a);
                self.arena.group(da, more)
            }
            Pattern::Attribute(nc, _) => match self.arena.name_matches(nc, ns, local) {
                true => self.arena.empty(),
                false => self.arena.not_allowed(),
            },
            _ => self.arena.not_allowed(),
        }
    }

    /// What the grammar wanted the attribute's value to be, worded from the declared
    /// datatype or, for an enumeration, the permitted literals.
    fn attribute_datatype(
        &self,
        p: PatternId,
        ns: Option<&str>,
        local: &str,
        value: &str,
    ) -> String {
        let mut kinds = Vec::new();
        self.collect_attribute_content(p, ns, local, &mut kinds);
        kinds.sort();
        kinds.dedup();
        match kinds.is_empty() {
            true => format!("{value:?}"),
            false => format!("{value:?} — expected {}", kinds.join(", ")),
        }
    }

    /// The content patterns declared for one attribute name, rendered as the
    /// datatype names and literals a reader can act on.
    fn collect_attribute_content(
        &self,
        p: PatternId,
        ns: Option<&str>,
        local: &str,
        out: &mut Vec<String>,
    ) {
        if out.len() > 16 {
            return; // a long list stops informing
        }
        match self.arena.pattern(p).clone() {
            Pattern::Choice(a, b) | Pattern::Group(a, b) | Pattern::Interleave(a, b) => {
                self.collect_attribute_content(a, ns, local, out);
                self.collect_attribute_content(b, ns, local, out);
            }
            Pattern::OneOrMore(a) | Pattern::After(a, _) => {
                self.collect_attribute_content(a, ns, local, out)
            }
            Pattern::Attribute(nc, content) if self.arena.name_matches(nc, ns, local) => {
                self.describe_value(content, out)
            }
            _ => {}
        }
    }

    /// One attribute content pattern as human-readable expectations.
    fn describe_value(&self, p: PatternId, out: &mut Vec<String>) {
        if out.len() > 16 {
            return;
        }
        match self.arena.pattern(p).clone() {
            Pattern::Choice(a, b) | Pattern::Group(a, b) | Pattern::Interleave(a, b) => {
                self.describe_value(a, out);
                self.describe_value(b, out);
            }
            Pattern::OneOrMore(a) | Pattern::List(a) => self.describe_value(a, out),
            Pattern::Data { datatype, .. } | Pattern::DataExcept { datatype, .. } => {
                out.push(datatype.name.clone())
            }
            Pattern::Value { value, .. } => out.push(format!("{value:?}")),
            _ => {}
        }
    }

    /// The paper's `valueMatch`: an attribute's content pattern accepts its
    /// value, where a nullable pattern also accepts whitespace-only.
    fn value_match(&mut self, p: PatternId, s: &str) -> bool {
        if self.arena.nullable(p) && s.chars().all(char::is_whitespace) {
            return true;
        }
        let d = self.text_deriv(p, s);
        self.arena.nullable(d)
    }

    /// The paper's `startTagOpenDeriv`: what remains after seeing `<name`.
    fn start_tag_open_deriv(&mut self, p: PatternId, ns: Option<&str>, local: &str) -> PatternId {
        let key = (p, ns.map(str::to_string), local.to_string());
        if let Some(hit) = self.open_cache.get(&key) {
            return *hit;
        }
        let result = match self.arena.pattern(p).clone() {
            Pattern::Choice(a, b) => {
                let x = self.start_tag_open_deriv(a, ns, local);
                let y = self.start_tag_open_deriv(b, ns, local);
                self.arena.choice(x, y)
            }
            Pattern::Element(nc, content) => {
                if self.arena.name_matches(nc, ns, local) {
                    let empty = self.arena.empty();
                    self.arena.after(content, empty)
                } else {
                    self.arena.not_allowed()
                }
            }
            Pattern::Interleave(a, b) => {
                let da = self.start_tag_open_deriv(a, ns, local);
                let left = self.apply_after(Cont::InterleaveRight(b), da);
                let db = self.start_tag_open_deriv(b, ns, local);
                let right = self.apply_after(Cont::InterleaveLeft(a), db);
                self.arena.choice(left, right)
            }
            Pattern::OneOrMore(a) => {
                let da = self.start_tag_open_deriv(a, ns, local);
                let more = self.arena.zero_or_more(a);
                self.apply_after(Cont::GroupRight(more), da)
            }
            Pattern::Group(a, b) => {
                let da = self.start_tag_open_deriv(a, ns, local);
                let first = self.apply_after(Cont::GroupRight(b), da);
                if self.arena.nullable(a) {
                    let db = self.start_tag_open_deriv(b, ns, local);
                    self.arena.choice(first, db)
                } else {
                    first
                }
            }
            Pattern::After(a, b) => {
                let da = self.start_tag_open_deriv(a, ns, local);
                self.apply_after(Cont::AfterRight(b), da)
            }
            _ => self.arena.not_allowed(),
        };
        self.open_cache.insert(key, result);
        result
    }

    /// The paper's `applyAfter`: push a continuation under every `after`.
    fn apply_after(&mut self, cont: Cont, p: PatternId) -> PatternId {
        match self.arena.pattern(p).clone() {
            Pattern::After(a, b) => {
                let applied = match cont {
                    Cont::GroupRight(q) => self.arena.group(b, q),
                    Cont::InterleaveRight(q) => self.arena.interleave(b, q),
                    Cont::InterleaveLeft(q) => self.arena.interleave(q, b),
                    Cont::AfterRight(q) => self.arena.after(b, q),
                };
                self.arena.after(a, applied)
            }
            Pattern::Choice(a, b) => {
                let x = self.apply_after(cont, a);
                let y = self.apply_after(cont, b);
                self.arena.choice(x, y)
            }
            _ => self.arena.not_allowed(),
        }
    }

    /// The paper's `startTagCloseDeriv`: no more attributes are coming, so any
    /// still-required attribute makes the pattern fail here.
    fn start_tag_close_deriv(&mut self, p: PatternId) -> PatternId {
        if let Some(hit) = self.close_cache.get(&p) {
            return *hit;
        }
        let result = match self.arena.pattern(p).clone() {
            Pattern::After(a, b) => {
                let da = self.start_tag_close_deriv(a);
                self.arena.after(da, b)
            }
            Pattern::Choice(a, b) => {
                let x = self.start_tag_close_deriv(a);
                let y = self.start_tag_close_deriv(b);
                self.arena.choice(x, y)
            }
            Pattern::Group(a, b) => {
                let x = self.start_tag_close_deriv(a);
                let y = self.start_tag_close_deriv(b);
                self.arena.group(x, y)
            }
            Pattern::Interleave(a, b) => {
                let x = self.start_tag_close_deriv(a);
                let y = self.start_tag_close_deriv(b);
                self.arena.interleave(x, y)
            }
            Pattern::OneOrMore(a) => {
                let x = self.start_tag_close_deriv(a);
                self.arena.one_or_more(x)
            }
            Pattern::Attribute(..) => self.arena.not_allowed(),
            _ => p,
        };
        self.close_cache.insert(p, result);
        result
    }

    /// The paper's `endTagDeriv`: the element's content is complete only if what
    /// is left to match is nullable.
    fn end_tag_deriv(&mut self, p: PatternId) -> PatternId {
        if let Some(hit) = self.end_cache.get(&p) {
            return *hit;
        }
        let result = match self.arena.pattern(p).clone() {
            Pattern::Choice(a, b) => {
                let x = self.end_tag_deriv(a);
                let y = self.end_tag_deriv(b);
                self.arena.choice(x, y)
            }
            Pattern::After(a, b) => {
                if self.arena.nullable(a) {
                    b
                } else {
                    self.arena.not_allowed()
                }
            }
            _ => self.arena.not_allowed(),
        };
        self.end_cache.insert(p, result);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::source::epub::xml::relaxng::pattern::{DatatypeName, NameClass};

    const XSD: &str = "http://www.w3.org/2001/XMLSchema-datatypes";

    /// `element <local> { content }`, in no namespace.
    fn element(a: &mut Arena, local: &str, content: PatternId) -> PatternId {
        let nc = a.intern_name(NameClass::Name {
            ns: None,
            local: local.into(),
        });
        a.intern(Pattern::Element(nc, content))
    }

    fn attribute(a: &mut Arena, local: &str, content: PatternId) -> PatternId {
        let nc = a.intern_name(NameClass::Name {
            ns: None,
            local: local.into(),
        });
        a.intern(Pattern::Attribute(nc, content))
    }

    fn xsd(a: &mut Arena, name: &str) -> PatternId {
        a.intern(Pattern::Data {
            datatype: DatatypeName {
                library: XSD.into(),
                name: name.into(),
            },
            params: Vec::new(),
        })
    }

    fn check(arena: &mut Arena, start: PatternId, xml: &str) -> Vec<Violation> {
        let doc = Document::parse(xml).expect("test documents are well-formed");
        Validator::new(arena).validate(&doc, start)
    }

    #[test]
    fn sequence_and_repetition() {
        let mut a = Arena::new();
        let empty = a.empty();
        let b = element(&mut a, "b", empty);
        let c = element(&mut a, "c", empty);
        let seq = a.group(b, c);
        let plus = a.one_or_more(seq);
        let root = element(&mut a, "a", plus);

        assert!(check(&mut a, root, "<a><b/><c/></a>").is_empty());
        assert!(check(&mut a, root, "<a><b/><c/><b/><c/></a>").is_empty());
        // Order matters, count matters, and the sequence must be complete.
        assert!(!check(&mut a, root, "<a><c/><b/></a>").is_empty());
        assert!(!check(&mut a, root, "<a><b/></a>").is_empty());
        assert!(!check(&mut a, root, "<a/>").is_empty());
    }

    #[test]
    fn interleave_accepts_any_order_but_still_counts() {
        let mut a = Arena::new();
        let empty = a.empty();
        let b = element(&mut a, "b", empty);
        let c = element(&mut a, "c", empty);
        let both = a.interleave(b, c);
        let root = element(&mut a, "a", both);

        assert!(check(&mut a, root, "<a><b/><c/></a>").is_empty());
        assert!(check(&mut a, root, "<a><c/><b/></a>").is_empty());
        assert!(
            !check(&mut a, root, "<a><b/></a>").is_empty(),
            "both required"
        );
        assert!(
            !check(&mut a, root, "<a><b/><c/><b/></a>").is_empty(),
            "once each"
        );
    }

    #[test]
    fn attributes_are_unordered_required_or_optional() {
        let mut a = Arena::new();
        let text = a.text();
        let empty = a.empty();
        let id = attribute(&mut a, "id", text);
        let class = attribute(&mut a, "class", text);
        let optional_class = a.optional(class);
        let attrs = a.group(id, optional_class);
        let content = a.group(attrs, empty);
        let root = element(&mut a, "a", content);

        assert!(check(&mut a, root, r#"<a id="x"/>"#).is_empty());
        assert!(check(&mut a, root, r#"<a id="x" class="y"/>"#).is_empty());
        assert!(
            check(&mut a, root, r#"<a class="y" id="x"/>"#).is_empty(),
            "attribute order is never significant"
        );
        let missing = check(&mut a, root, "<a/>");
        assert!(!missing.is_empty(), "id is required");
        assert!(
            missing[0].message.contains("required attribute"),
            "{missing:?}"
        );
        let extra = check(&mut a, root, r#"<a id="x" bogus="1"/>"#);
        assert!(!extra.is_empty(), "an undeclared attribute is rejected");
        assert!(extra[0].message.contains("bogus"), "{extra:?}");
    }

    #[test]
    fn datatypes_decide_attribute_values() {
        let mut a = Arena::new();
        let empty = a.empty();
        let id_type = xsd(&mut a, "ID");
        let id = attribute(&mut a, "id", id_type);
        let content = a.group(id, empty);
        let root = element(&mut a, "a", content);

        assert!(check(&mut a, root, r#"<a id="chapter-1"/>"#).is_empty());
        assert!(
            check(&mut a, root, r#"<a id="  padded  "/>"#).is_empty(),
            "ID collapses whitespace, so a padded value is valid"
        );
        assert!(
            !check(&mut a, root, r#"<a id="1first"/>"#).is_empty(),
            "an NCName cannot start with a digit"
        );
    }

    #[test]
    fn text_is_allowed_only_where_the_pattern_says() {
        let mut a = Arena::new();
        let text = a.text();
        let empty = a.empty();
        let with_text = element(&mut a, "t", text);
        let without = element(&mut a, "e", empty);
        let both = a.group(with_text, without);
        let root = element(&mut a, "a", both);

        assert!(check(&mut a, root, "<a><t>hello</t><e/></a>").is_empty());
        assert!(
            check(&mut a, root, "<a><t/><e>\n  </e></a>").is_empty(),
            "whitespace-only content also matches an empty pattern"
        );
        let bad = check(&mut a, root, "<a><t/><e>oops</e></a>");
        assert!(!bad.is_empty(), "text where the pattern allows none");
    }

    #[test]
    fn a_recursive_grammar_terminates() {
        // element div { (div | text)* } — the shape every real content grammar
        // has, and the reason patterns live in an arena.
        let mut a = Arena::new();
        let div = a.reserve();
        let text = a.text();
        let branch = a.choice(div, text);
        let content = a.zero_or_more(branch);
        let nc = a.intern_name(NameClass::Name {
            ns: None,
            local: "div".into(),
        });
        a.fill(div, Pattern::Element(nc, content));

        assert!(check(&mut a, div, "<div/>").is_empty());
        assert!(check(&mut a, div, "<div><div><div/></div></div>").is_empty());
        assert!(check(&mut a, div, "<div>text<div/>more</div>").is_empty());
        assert!(!check(&mut a, div, "<div><span/></div>").is_empty());
    }

    #[test]
    fn namespaces_are_matched_by_uri() {
        let mut a = Arena::new();
        let empty = a.empty();
        let nc = a.intern_name(NameClass::Name {
            ns: Some("urn:x".into()),
            local: "a".into(),
        });
        let root = a.intern(Pattern::Element(nc, empty));

        assert!(check(&mut a, root, r#"<a xmlns="urn:x"/>"#).is_empty());
        assert!(
            check(&mut a, root, r#"<p:a xmlns:p="urn:x"/>"#).is_empty(),
            "the prefix is irrelevant"
        );
        assert!(!check(&mut a, root, r#"<a xmlns="urn:other"/>"#).is_empty());
        assert!(
            !check(&mut a, root, "<a/>").is_empty(),
            "no namespace differs"
        );
    }

    #[test]
    fn the_diagnostic_names_what_was_expected() {
        let mut a = Arena::new();
        let empty = a.empty();
        let b = element(&mut a, "b", empty);
        let c = element(&mut a, "c", empty);
        let either = a.choice(b, c);
        let root = element(&mut a, "a", either);

        let v = check(&mut a, root, "<a><zz/></a>");
        assert_eq!(v.len(), 1);
        assert!(v[0].message.contains("\"zz\""), "{:?}", v[0].message);
        assert!(v[0].message.contains("\"b\""), "{:?}", v[0].message);
        assert!(v[0].message.contains("\"c\""), "{:?}", v[0].message);
    }
}
