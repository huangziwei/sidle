//! Compile a RELAX NG grammar written in the **XML syntax** (`.rng`) into the
//! pattern model.

use std::borrow::Cow;
use std::collections::HashMap;

use super::pattern::{Arena, DatatypeName, NameClass, NameClassId, Pattern, PatternId};
use crate::validate::source::epub::xml::tree::{Document, NodeId, NodeKind};

/// The RELAX NG structure namespace. An element in any other namespace is an
/// annotation and is ignored (§4.1).
pub const RNG_NS: &str = "http://relaxng.org/ns/structure/1.0";

/// Supplies the text of a grammar file referenced by `href`, so `include` and
/// `externalRef` can be followed. Paths are resolved relative to the referring
/// file, exactly as the specification requires.
pub trait Resolver {
    /// The grammar at `href`, resolved against `base` (the referring file's
    /// path). Returns the resolved path and its content.
    fn resolve(&self, base: &str, href: &str) -> Option<(String, String)>;
}

/// A [`Resolver`] over an in-memory map of path → content, which is how the
/// vendored schemas are carried (they are compiled into the binary; nothing is
/// read from disk at runtime).
pub struct MapResolver<'a>(pub &'a HashMap<String, String>);

impl Resolver for MapResolver<'_> {
    fn resolve(&self, base: &str, href: &str) -> Option<(String, String)> {
        let path = join_relative(base, href);
        self.0.get(&path).map(|c| (path, c.clone()))
    }
}

/// The XML syntax of a grammar file, translating it from the compact syntax
/// first when its path says that is what it is.
fn xml_syntax<'a>(path: &str, source: &'a str) -> Result<Cow<'a, str>, CompileError> {
    match path.ends_with(".rnc") {
        true => Ok(Cow::Owned(super::rnc::translate(path, source)?)),
        false => Ok(Cow::Borrowed(source)),
    }
}

/// Resolve `href` against the directory of `base`, collapsing `.`/`..`.
pub fn join_relative(base: &str, href: &str) -> String {
    let dir = base.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for segment in href.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// What a compilation failed on. A grammar that will not compile is a defect in
/// this port, never in the document being validated, so it is reported as an
/// error rather than turned into a finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError(pub String);

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The inherited attributes of §4.3 and §4.7 — `datatypeLibrary` and `ns` flow
/// down the syntax tree unless an element overrides them.
#[derive(Debug, Clone, Default)]
struct Ctx {
    datatype_library: String,
    ns: Option<String>,
    /// The grammar file this element came from, so a nested `href` resolves
    /// against the right base.
    base: String,
}

/// One `<grammar>`'s definitions: `name → pattern id`, with the reserved ids
/// that let a `ref` be compiled before its `define`.
#[derive(Debug, Default)]
struct Scope {
    slots: HashMap<String, PatternId>,
    /// Bodies collected per name, to be combined per §4.15.
    bodies: HashMap<String, (Option<String>, Vec<PatternId>)>,
    start: Option<PatternId>,
}

/// Compiles grammars into one shared [`Arena`].
pub struct Compiler<'a, R: Resolver> {
    arena: &'a mut Arena,
    resolver: &'a R,
    /// The enclosing `<grammar>` scopes; `parentRef` reaches the one below the
    /// top (§4.16).
    scopes: Vec<Scope>,
    /// Files already being included, so a cyclic `include` is an error rather
    /// than a hang.
    including: Vec<String>,
}

impl<'a, R: Resolver> Compiler<'a, R> {
    pub fn new(arena: &'a mut Arena, resolver: &'a R) -> Self {
        Compiler {
            arena,
            resolver,
            scopes: Vec::new(),
            including: Vec::new(),
        }
    }

    /// Compile the grammar in `source` (whose path is `base`) and return its
    /// start pattern. A compact-syntax file is translated first.
    pub fn compile(&mut self, base: &str, source: &str) -> Result<PatternId, CompileError> {
        let source = xml_syntax(base, source)?;
        let doc = Document::parse(&source)
            .map_err(|e| CompileError(format!("{base}: not well-formed: {e}")))?;
        let root = doc
            .root_element()
            .ok_or_else(|| CompileError(format!("{base}: empty grammar")))?;
        let ctx = Ctx {
            datatype_library: attr(&doc, root, "datatypeLibrary").unwrap_or_default(),
            ns: attr(&doc, root, "ns"),
            base: base.to_string(),
        };
        match local_name(&doc, root) {
            Some("grammar") => self.grammar(&doc, root, &ctx),
            // A grammar file may also be a bare pattern (`<element>…`), which is
            // what `externalRef` most often points at.
            Some(_) => self.pattern(&doc, root, &ctx),
            None => Err(CompileError(format!(
                "{base}: root is not a RELAX NG element"
            ))),
        }
    }

    /// `<grammar>`: collect every `define`/`start`, following `include`, then
    /// fill the reserved slots.
    fn grammar(
        &mut self,
        doc: &Document,
        node: NodeId,
        ctx: &Ctx,
    ) -> Result<PatternId, CompileError> {
        self.scopes.push(Scope::default());
        let result = self.grammar_body(doc, node, ctx).and_then(|()| {
            let scope = self.scopes.last_mut().expect("pushed above");
            let names: Vec<String> = scope.bodies.keys().cloned().collect();
            for name in names {
                let (combine, bodies) = self.scopes.last().unwrap().bodies[&name].clone();
                let slot = self.scopes.last().unwrap().slots[&name];
                let combined = self.combine(combine.as_deref(), &bodies)?;
                // Point at the body rather than copying it: the body may itself
                // be a slot this loop has not filled yet.
                self.arena.fill(slot, Pattern::Ref(combined));
            }
            // A `ref` to a name no `define` ever supplies leaves its reserved
            let scope = self.scopes.last().unwrap();
            let dangling: Vec<&String> = scope
                .slots
                .keys()
                .filter(|name| !scope.bodies.contains_key(*name))
                .collect();
            if !dangling.is_empty() {
                let mut names: Vec<&str> = dangling.iter().map(|n| n.as_str()).collect();
                names.sort_unstable();
                return Err(CompileError(format!(
                    "reference(s) to undefined pattern(s): {}",
                    names.join(", ")
                )));
            }
            scope
                .start
                .ok_or_else(|| CompileError("grammar has no <start>".into()))
        });
        let scope = self.scopes.pop();
        // A nested grammar's definitions go out of scope with it, but its start
        // pattern lives on in the arena.
        drop(scope);
        result
    }

    /// Walk a grammar's content, honouring `include` and `div` (§4.9, §4.6).
    fn grammar_body(
        &mut self,
        doc: &Document,
        node: NodeId,
        ctx: &Ctx,
    ) -> Result<(), CompileError> {
        for child in children(doc, node) {
            let ctx = inherit(doc, child, ctx);
            match local_name(doc, child) {
                Some("start") => {
                    let combine = attr(doc, child, "combine");
                    let body = self.pattern_group(doc, child, &ctx)?;
                    let scope = self.scopes.last_mut().expect("in a grammar");
                    scope.start = Some(match (scope.start, combine.as_deref()) {
                        (None, _) => body,
                        (Some(prev), Some("interleave")) => self.arena.interleave(prev, body),
                        (Some(prev), _) => self.arena.choice(prev, body),
                    });
                }
                Some("define") => {
                    let name = attr(doc, child, "name")
                        .ok_or_else(|| CompileError("<define> has no name".into()))?;
                    let combine = attr(doc, child, "combine");
                    let body = self.pattern_group(doc, child, &ctx)?;
                    self.add_define(&name, combine, body);
                }
                Some("div") => self.grammar_body(doc, child, &ctx)?,
                Some("include") => self.include(doc, child, &ctx)?,
                Some(other) => {
                    return Err(CompileError(format!("<{other}> is not grammar content")));
                }
                None => {}
            }
        }
        Ok(())
    }

    /// §4.15: repeated `define`s of one name are combined, and the *overriding*
    /// definitions inside an `<include>` replace the included ones entirely.
    fn add_define(&mut self, name: &str, combine: Option<String>, body: PatternId) {
        let arena = &mut *self.arena;
        let scope = self.scopes.last_mut().expect("in a grammar");
        scope
            .slots
            .entry(name.to_string())
            .or_insert_with(|| arena.reserve());
        let entry = scope
            .bodies
            .entry(name.to_string())
            .or_insert_with(|| (None, Vec::new()));
        if entry.0.is_none() {
            entry.0 = combine;
        }
        entry.1.push(body);
    }

    /// Combine the bodies of one `define` name (§4.15).
    fn combine(
        &mut self,
        how: Option<&str>,
        bodies: &[PatternId],
    ) -> Result<PatternId, CompileError> {
        let mut iter = bodies.iter().copied();
        let mut acc = iter
            .next()
            .ok_or_else(|| CompileError("define with no body".into()))?;
        for next in iter {
            acc = match how {
                Some("interleave") => self.arena.interleave(acc, next),
                _ => self.arena.choice(acc, next),
            };
        }
        Ok(acc)
    }

    /// `<include href="…">`: compile the referenced grammar's body into *this*
    /// scope, with any `define` inside the `include` element overriding it.
    fn include(&mut self, doc: &Document, node: NodeId, ctx: &Ctx) -> Result<(), CompileError> {
        let href =
            attr(doc, node, "href").ok_or_else(|| CompileError("<include> has no href".into()))?;
        let (path, source) = self.resolver.resolve(&ctx.base, &href).ok_or_else(|| {
            CompileError(format!("cannot resolve include {href:?} from {}", ctx.base))
        })?;
        if self.including.contains(&path) {
            return Err(CompileError(format!("cyclic include of {path}")));
        }

        // The overrides are compiled first: a name defined here must win, and
        // `add_define` keeps the first `combine` it sees.
        let overridden: Vec<String> = children(doc, node)
            .filter(|c| local_name(doc, *c) == Some("define"))
            .filter_map(|c| attr(doc, c, "name"))
            .collect();
        self.grammar_body(doc, node, ctx)?;

        let source = xml_syntax(&path, &source)?;
        let included = Document::parse(&source)
            .map_err(|e| CompileError(format!("{path}: not well-formed: {e}")))?;
        let root = included
            .root_element()
            .ok_or_else(|| CompileError(format!("{path}: empty grammar")))?;
        let inner_ctx = Ctx {
            datatype_library: attr(&included, root, "datatypeLibrary")
                .unwrap_or_else(|| ctx.datatype_library.clone()),
            ns: attr(&included, root, "ns").or_else(|| ctx.ns.clone()),
            base: path.clone(),
        };
        self.including.push(path);
        let result = self.include_body(&included, root, &inner_ctx, &overridden);
        self.including.pop();
        result
    }

    /// Like [`grammar_body`](Self::grammar_body) but skipping the names the
    /// including grammar overrode (§4.6).
    fn include_body(
        &mut self,
        doc: &Document,
        node: NodeId,
        ctx: &Ctx,
        overridden: &[String],
    ) -> Result<(), CompileError> {
        for child in children(doc, node) {
            let ctx = inherit(doc, child, ctx);
            match local_name(doc, child) {
                Some("define") => {
                    let name = attr(doc, child, "name")
                        .ok_or_else(|| CompileError("<define> has no name".into()))?;
                    if overridden.contains(&name) {
                        continue; // replaced by the including grammar
                    }
                    let combine = attr(doc, child, "combine");
                    let body = self.pattern_group(doc, child, &ctx)?;
                    self.add_define(&name, combine, body);
                }
                Some("start") => {
                    let body = self.pattern_group(doc, child, &ctx)?;
                    let scope = self.scopes.last_mut().expect("in a grammar");
                    if scope.start.is_none() {
                        scope.start = Some(body);
                    }
                }
                Some("div") => self.include_body(doc, child, &ctx, overridden)?,
                Some("include") => self.include(doc, child, &ctx)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// The children of `node` as one pattern: `group` for several, and the
    /// single child unchanged for one (§4.10).
    fn pattern_group(
        &mut self,
        doc: &Document,
        node: NodeId,
        ctx: &Ctx,
    ) -> Result<PatternId, CompileError> {
        let kids: Vec<NodeId> = children(doc, node).collect();
        self.fold(doc, &kids, ctx, |arena, a, b| arena.group(a, b))
    }

    /// Fold a node's children into a binary tree with `combine` (§4.10).
    fn fold(
        &mut self,
        doc: &Document,
        kids: &[NodeId],
        ctx: &Ctx,
        combine: fn(&mut Arena, PatternId, PatternId) -> PatternId,
    ) -> Result<PatternId, CompileError> {
        let mut acc: Option<PatternId> = None;
        for kid in kids {
            let ctx = inherit(doc, *kid, ctx);
            let p = self.pattern(doc, *kid, &ctx)?;
            acc = Some(match acc {
                None => p,
                Some(prev) => combine(self.arena, prev, p),
            });
        }
        Ok(acc.unwrap_or_else(|| self.arena.empty()))
    }

    /// Compile one pattern element.
    fn pattern(
        &mut self,
        doc: &Document,
        node: NodeId,
        ctx: &Ctx,
    ) -> Result<PatternId, CompileError> {
        let Some(name) = local_name(doc, node) else {
            return Err(CompileError("not a RELAX NG element".into()));
        };
        let kids: Vec<NodeId> = children(doc, node).collect();
        match name {
            "empty" => Ok(self.arena.empty()),
            "notAllowed" => Ok(self.arena.not_allowed()),
            "text" => Ok(self.arena.text()),
            "group" => self.fold(doc, &kids, ctx, |a, x, y| a.group(x, y)),
            "choice" => self.fold(doc, &kids, ctx, |a, x, y| a.choice(x, y)),
            "interleave" => self.fold(doc, &kids, ctx, |a, x, y| a.interleave(x, y)),
            "optional" => {
                let inner = self.pattern_group(doc, node, ctx)?;
                Ok(self.arena.optional(inner))
            }
            "zeroOrMore" => {
                let inner = self.pattern_group(doc, node, ctx)?;
                Ok(self.arena.zero_or_more(inner))
            }
            "oneOrMore" => {
                let inner = self.pattern_group(doc, node, ctx)?;
                Ok(self.arena.one_or_more(inner))
            }
            // §4.11: `mixed { p }` is `interleave { p, text }`.
            "mixed" => {
                let inner = self.pattern_group(doc, node, ctx)?;
                let text = self.arena.text();
                Ok(self.arena.interleave(inner, text))
            }
            "list" => {
                let inner = self.pattern_group(doc, node, ctx)?;
                Ok(self.arena.intern(Pattern::List(inner)))
            }
            "element" | "attribute" => self.named(doc, node, ctx, name == "element"),
            "ref" => {
                let target = attr(doc, node, "name")
                    .ok_or_else(|| CompileError("<ref> has no name".into()))?;
                self.reference(&target, false)
            }
            "parentRef" => {
                let target = attr(doc, node, "name")
                    .ok_or_else(|| CompileError("<parentRef> has no name".into()))?;
                self.reference(&target, true)
            }
            "value" => {
                // §4.12: a `<value>` with no `type` is `token` in the *built-in*
                let datatype = match attr(doc, node, "type") {
                    None => DatatypeName {
                        library: String::new(),
                        name: "token".into(),
                    },
                    Some(_) => self.datatype(doc, node, ctx, "token"),
                };
                let value = doc.string_value(node);
                Ok(self.arena.intern(Pattern::Value { datatype, value }))
            }
            "data" => {
                let datatype = self.datatype(doc, node, ctx, "string");
                let params = kids
                    .iter()
                    .filter(|k| local_name(doc, **k) == Some("param"))
                    .filter_map(|k| attr(doc, *k, "name").map(|n| (n, doc.string_value(*k))))
                    .collect();
                let except = kids.iter().find(|k| local_name(doc, **k) == Some("except"));
                match except {
                    None => Ok(self.arena.intern(Pattern::Data { datatype, params })),
                    Some(e) => {
                        let inner = self.pattern_choice(doc, *e, ctx)?;
                        Ok(self.arena.intern(Pattern::DataExcept {
                            datatype,
                            params,
                            except: inner,
                        }))
                    }
                }
            }
            "grammar" => self.grammar(doc, node, ctx),
            "externalRef" => {
                let href = attr(doc, node, "href")
                    .ok_or_else(|| CompileError("<externalRef> has no href".into()))?;
                let (path, source) = self.resolver.resolve(&ctx.base, &href).ok_or_else(|| {
                    CompileError(format!(
                        "cannot resolve externalRef {href:?} from {}",
                        ctx.base
                    ))
                })?;
                self.compile(&path, &source)
            }
            other => Err(CompileError(format!("unknown pattern <{other}>"))),
        }
    }

    /// The children of `node` combined with `choice` — what `<except>` and the
    /// name-class contexts want.
    fn pattern_choice(
        &mut self,
        doc: &Document,
        node: NodeId,
        ctx: &Ctx,
    ) -> Result<PatternId, CompileError> {
        let kids: Vec<NodeId> = children(doc, node).collect();
        self.fold(doc, &kids, ctx, |a, x, y| a.choice(x, y))
    }

    /// `<element>` / `<attribute>`: a name class plus a content pattern.
    fn named(
        &mut self,
        doc: &Document,
        node: NodeId,
        ctx: &Ctx,
        is_element: bool,
    ) -> Result<PatternId, CompileError> {
        let nc = self.name_class(doc, node, ctx)?;
        // The content is every child except the leading name class — which §4.7
        // says is the *first* child, and only when there is no `name` attribute.
        // Dropping every name-class-looking child instead would swallow content:
        let mut kids: Vec<NodeId> = children(doc, node).collect();
        if attr(doc, node, "name").is_none() && !kids.is_empty() {
            kids.remove(0);
        }
        let content = if is_element {
            self.fold(doc, &kids, ctx, |a, x, y| a.group(x, y))?
        } else {
            // An attribute with no content pattern accepts any string (§4.14).
            match kids.is_empty() {
                true => self.arena.text(),
                false => self.fold(doc, &kids, ctx, |a, x, y| a.group(x, y))?,
            }
        };
        Ok(self.arena.intern(if is_element {
            Pattern::Element(nc, content)
        } else {
            Pattern::Attribute(nc, content)
        }))
    }

    /// The name class of an `<element>`/`<attribute>`: either the `name`
    /// attribute (§4.7) or the name-class child.
    fn name_class(
        &mut self,
        doc: &Document,
        node: NodeId,
        ctx: &Ctx,
    ) -> Result<NameClassId, CompileError> {
        if let Some(name) = attr(doc, node, "name") {
            return Ok(self.qname(doc, node, &name, &unprefixed_ns(doc, node, ctx)));
        }
        let child = children(doc, node)
            .next()
            .filter(|c| is_name_class(doc, *c))
            .ok_or_else(|| CompileError("<element>/<attribute> has no name class".into()))?;
        self.name_class_element(doc, child, ctx)
    }

    fn name_class_element(
        &mut self,
        doc: &Document,
        node: NodeId,
        ctx: &Ctx,
    ) -> Result<NameClassId, CompileError> {
        let ctx = inherit(doc, node, ctx);
        match local_name(doc, node) {
            Some("name") => {
                let text = doc.string_value(node);
                Ok(self.qname(doc, node, text.trim(), &unprefixed_ns(doc, node, &ctx)))
            }
            Some("anyName") => {
                let except = children(doc, node).find(|c| local_name(doc, *c) == Some("except"));
                match except {
                    None => Ok(self.arena.intern_name(NameClass::AnyName)),
                    Some(e) => {
                        let inner = self.name_class_choice(doc, e, &ctx)?;
                        Ok(self.arena.intern_name(NameClass::AnyNameExcept(inner)))
                    }
                }
            }
            Some("nsName") => {
                // `ns=""` is how "no namespace" is written; the pattern model
                // spells that `None`, and an unnormalised `Some("")` would match
                // nothing at all.
                let ns = ctx.ns.clone().filter(|u| !u.is_empty());
                let except = children(doc, node).find(|c| local_name(doc, *c) == Some("except"));
                match except {
                    None => Ok(self.arena.intern_name(NameClass::NsName(ns))),
                    Some(e) => {
                        let inner = self.name_class_choice(doc, e, &ctx)?;
                        Ok(self.arena.intern_name(NameClass::NsNameExcept(ns, inner)))
                    }
                }
            }
            Some("choice") => self.name_class_choice(doc, node, &ctx),
            other => Err(CompileError(format!("{other:?} is not a name class"))),
        }
    }

    fn name_class_choice(
        &mut self,
        doc: &Document,
        node: NodeId,
        ctx: &Ctx,
    ) -> Result<NameClassId, CompileError> {
        let mut acc: Option<NameClassId> = None;
        for child in children(doc, node) {
            let nc = self.name_class_element(doc, child, ctx)?;
            acc = Some(match acc {
                None => nc,
                Some(prev) => self.arena.intern_name(NameClass::Choice(prev, nc)),
            });
        }
        acc.ok_or_else(|| CompileError("empty name class".into()))
    }

    /// §4.8: a `QName` resolves its prefix through the schema document's own namespace
    /// declarations; the name sits in an attribute value. Unbound leaves no namespace.
    fn qname(
        &mut self,
        doc: &Document,
        node: NodeId,
        name: &str,
        unprefixed: &Option<String>,
    ) -> NameClassId {
        let (prefix, local) = match name.split_once(':') {
            Some((p, l)) => (Some(p), l),
            None => (None, name),
        };
        let ns = match prefix {
            Some(prefix) => doc.in_scope_namespace(node, prefix).map(str::to_string),
            None => unprefixed.clone(),
        };
        let ns = ns.filter(|u| !u.is_empty());
        self.arena.intern_name(NameClass::Name {
            ns,
            local: local.to_string(),
        })
    }

    /// The datatype of a `<value>`/`<data>`, with the inherited library (§4.3)
    /// and the default type name for the element.
    fn datatype(&mut self, doc: &Document, node: NodeId, ctx: &Ctx, default: &str) -> DatatypeName {
        DatatypeName {
            library: attr(doc, node, "datatypeLibrary")
                .unwrap_or_else(|| ctx.datatype_library.clone()),
            name: attr(doc, node, "type").unwrap_or_else(|| default.to_string()),
        }
    }

    /// Resolve a `ref`/`parentRef` to its slot, reserving one if the `define`
    /// has not been seen yet (§4.16).
    fn reference(&mut self, name: &str, parent: bool) -> Result<PatternId, CompileError> {
        let depth = self.scopes.len();
        let index = if parent {
            depth.checked_sub(2).ok_or_else(|| {
                CompileError(format!(
                    "<parentRef name={name:?}> outside a nested grammar"
                ))
            })?
        } else {
            depth
                .checked_sub(1)
                .ok_or_else(|| CompileError(format!("<ref name={name:?}> outside a grammar")))?
        };
        let arena = &mut *self.arena;
        Ok(*self.scopes[index]
            .slots
            .entry(name.to_string())
            .or_insert_with(|| arena.reserve()))
    }
}

/// The RELAX NG element children of `node`, with `div` flattened (§4.9) and
/// foreign-namespace annotations skipped (§4.1).
fn children(doc: &Document, node: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    let mut out = Vec::new();
    collect_children(doc, node, &mut out);
    out.into_iter()
}

fn collect_children(doc: &Document, node: NodeId, out: &mut Vec<NodeId>) {
    for child in doc.element_children(node) {
        match local_name(doc, child) {
            Some("div") => collect_children(doc, child, out),
            Some(_) => out.push(child),
            None => {} // an annotation in a foreign namespace
        }
    }
}

/// The local name of `node` if it is in the RELAX NG namespace.
fn local_name(doc: &Document, node: NodeId) -> Option<&str> {
    let element = doc.element(node)?;
    let (ns, local) = doc.expanded(&element.name);
    (ns == Some(RNG_NS)).then_some(local)
}

/// A no-namespace attribute of a grammar element, trimmed (§4.2).
fn attr(doc: &Document, node: NodeId, name: &str) -> Option<String> {
    doc.attr(node, None, name).map(|v| v.trim().to_string())
}

fn is_attribute(doc: &Document, node: NodeId) -> bool {
    local_name(doc, node) == Some("attribute")
}

/// The namespace an *unprefixed* name at `node` belongs to.
fn unprefixed_ns(doc: &Document, node: NodeId, ctx: &Ctx) -> Option<String> {
    let mut current = Some(node);
    while let Some(id) = current {
        if let Some(ns) = attr(doc, id, "ns") {
            return Some(ns);
        }
        if is_attribute(doc, id) {
            return None; // reached the attribute pattern with nothing explicit
        }
        current = doc.node(id).parent;
    }
    ctx.ns.clone()
}

fn is_name_class(doc: &Document, node: NodeId) -> bool {
    matches!(
        local_name(doc, node),
        Some("name") | Some("anyName") | Some("nsName") | Some("choice")
    )
}

/// Push `datatypeLibrary`/`ns`/base down to a child (§4.3, §4.7).
fn inherit(doc: &Document, node: NodeId, ctx: &Ctx) -> Ctx {
    Ctx {
        datatype_library: attr(doc, node, "datatypeLibrary")
            .unwrap_or_else(|| ctx.datatype_library.clone()),
        ns: attr(doc, node, "ns").or_else(|| ctx.ns.clone()),
        base: ctx.base.clone(),
    }
}

/// Whether a node is character data, used by the tests below.
#[allow(dead_code)]
fn is_text(doc: &Document, node: NodeId) -> bool {
    matches!(doc.node(node).kind, NodeKind::Text(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::source::epub::xml::relaxng::derive::Validator;

    fn compile(files: &[(&str, &str)]) -> (Arena, PatternId) {
        let map: HashMap<String, String> = files
            .iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect();
        let mut arena = Arena::new();
        let start = {
            let resolver = MapResolver(&map);
            let mut compiler = Compiler::new(&mut arena, &resolver);
            let (path, source) = (files[0].0, files[0].1);
            compiler.compile(path, source).expect("grammar compiles")
        };
        (arena, start)
    }

    fn valid(arena: &mut Arena, start: PatternId, xml: &str) -> bool {
        let doc = Document::parse(xml).expect("well-formed test document");
        Validator::new(arena).validate(&doc, start).is_empty()
    }

    #[test]
    fn compiles_a_grammar_with_defines_and_refs() {
        let (mut arena, start) = compile(&[(
            "g.rng",
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                 <start><ref name="doc"/></start>
                 <define name="doc">
                   <element name="doc"><oneOrMore><ref name="item"/></oneOrMore></element>
                 </define>
                 <define name="item"><element name="item"><text/></element></define>
               </grammar>"#,
        )]);
        assert!(valid(&mut arena, start, "<doc><item>a</item></doc>"));
        assert!(valid(
            &mut arena,
            start,
            "<doc><item>a</item><item>b</item></doc>"
        ));
        assert!(!valid(&mut arena, start, "<doc/>"), "oneOrMore needs one");
        assert!(!valid(&mut arena, start, "<doc><other/></doc>"));
    }

    #[test]
    fn sugar_reduces_to_the_primitives() {
        let (mut arena, start) = compile(&[(
            "g.rng",
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                 <start>
                   <element name="a">
                     <optional><attribute name="id"/></optional>
                     <zeroOrMore><element name="b"><empty/></element></zeroOrMore>
                     <mixed><element name="c"><empty/></element></mixed>
                   </element>
                 </start>
               </grammar>"#,
        )]);
        assert!(valid(&mut arena, start, "<a><c/></a>"));
        assert!(valid(&mut arena, start, r#"<a id="x"><b/><b/><c/></a>"#));
        assert!(
            valid(&mut arena, start, "<a>text<c/>more</a>"),
            "mixed allows text"
        );
        assert!(!valid(&mut arena, start, "<a/>"), "c is required");
    }

    #[test]
    fn include_merges_and_overrides_definitions() {
        let (mut arena, start) = compile(&[
            (
                "main.rng",
                r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                     <include href="base.rng">
                       <define name="body"><element name="body"><element name="p"><text/></element></element></define>
                     </include>
                   </grammar>"#,
            ),
            (
                "base.rng",
                r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                     <start><element name="html"><ref name="body"/></element></start>
                     <define name="body"><element name="body"><empty/></element></define>
                   </grammar>"#,
            ),
        ]);
        // The override wins: <body> must now contain a <p>.
        assert!(valid(
            &mut arena,
            start,
            "<html><body><p>x</p></body></html>"
        ));
        assert!(!valid(&mut arena, start, "<html><body/></html>"));
    }

    #[test]
    fn combine_merges_repeated_defines() {
        let (mut arena, start) = compile(&[(
            "g.rng",
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                 <start><element name="a"><ref name="inline"/></element></start>
                 <define name="inline"><element name="b"><empty/></element></define>
                 <define name="inline" combine="choice"><element name="i"><empty/></element></define>
               </grammar>"#,
        )]);
        assert!(valid(&mut arena, start, "<a><b/></a>"));
        assert!(valid(&mut arena, start, "<a><i/></a>"));
        assert!(!valid(&mut arena, start, "<a><x/></a>"));
    }

    #[test]
    fn namespaces_come_from_the_ns_attribute() {
        let (mut arena, start) = compile(&[(
            "g.rng",
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0" ns="urn:x">
                 <start><element name="a"><attribute name="id"/></element></start>
               </grammar>"#,
        )]);
        assert!(valid(&mut arena, start, r#"<a xmlns="urn:x" id="1"/>"#));
        assert!(
            !valid(&mut arena, start, r#"<a id="1"/>"#),
            "element takes ns"
        );
        // An unprefixed attribute name is in NO namespace even under `ns`.
        assert!(!valid(
            &mut arena,
            start,
            r#"<a xmlns="urn:x" xmlns:p="urn:x" p:id="1"/>"#
        ));
    }

    /// A prefixed `name="…"` is written in an *attribute value*, so the XML
    #[test]
    fn a_prefixed_name_resolves_through_the_grammars_own_declarations() {
        let (mut arena, start) = compile(&[(
            "g.rng",
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0" ns="urn:x"
                        xmlns:xlink="urn:xlink">
                 <start>
                   <element name="a">
                     <optional><attribute name="xml:lang"/></optional>
                     <optional><attribute name="xlink:href"/></optional>
                     <optional><attribute name="lang"/></optional>
                   </element>
                 </start>
               </grammar>"#,
        )]);
        assert!(valid(
            &mut arena,
            start,
            r#"<a xmlns="urn:x" xml:lang="en" lang="en"/>"#
        ));
        assert!(valid(
            &mut arena,
            start,
            r#"<a xmlns="urn:x" xmlns:l="urn:xlink" l:href="x"/>"#
        ));
        // The grammar's `ns` is not what a prefix resolves to.
        assert!(!valid(
            &mut arena,
            start,
            r#"<a xmlns="urn:x" xmlns:p="urn:x" p:lang="en"/>"#
        ));
    }

    /// An unprefixed attribute name is in no namespace — unless the grammar
    /// writes an `ns` of its own, which is the other way real schemas spell
    /// `xml:lang`.
    #[test]
    fn an_explicit_ns_puts_an_unprefixed_attribute_in_a_namespace() {
        let (mut arena, start) = compile(&[(
            "g.rng",
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0" ns="urn:x">
                 <start>
                   <element name="a">
                     <optional>
                       <attribute name="lang" ns="http://www.w3.org/XML/1998/namespace"/>
                     </optional>
                     <optional>
                       <attribute><name ns="urn:y">via-name-class</name></attribute>
                     </optional>
                   </element>
                 </start>
               </grammar>"#,
        )]);
        assert!(valid(
            &mut arena,
            start,
            r#"<a xmlns="urn:x" xml:lang="en"/>"#
        ));
        assert!(
            !valid(&mut arena, start, r#"<a xmlns="urn:x" lang="en"/>"#),
            "the explicit ns means the no-namespace spelling no longer matches"
        );
        assert!(valid(
            &mut arena,
            start,
            r#"<a xmlns="urn:x" xmlns:y="urn:y" y:via-name-class="1"/>"#
        ));
    }

    #[test]
    fn datatypes_and_values_come_through() {
        let (mut arena, start) = compile(&[(
            "g.rng",
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0"
                        datatypeLibrary="http://www.w3.org/2001/XMLSchema-datatypes">
                 <start>
                   <element name="a">
                     <attribute name="id"><data type="ID"/></attribute>
                     <attribute name="kind"><choice><value>one</value><value>two</value></choice></attribute>
                   </element>
                 </start>
               </grammar>"#,
        )]);
        assert!(valid(&mut arena, start, r#"<a id="x" kind="one"/>"#));
        assert!(!valid(&mut arena, start, r#"<a id="1bad" kind="one"/>"#));
        assert!(!valid(&mut arena, start, r#"<a id="x" kind="three"/>"#));
    }

    #[test]
    fn annotations_and_div_are_transparent() {
        let (mut arena, start) = compile(&[(
            "g.rng",
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0"
                        xmlns:a="http://relaxng.org/ns/compatibility/annotations/1.0">
                 <div>
                   <start><element name="a"><a:documentation>ignored</a:documentation><empty/></element></start>
                 </div>
               </grammar>"#,
        )]);
        assert!(valid(&mut arena, start, "<a/>"));
    }

    #[test]
    fn relative_hrefs_resolve_against_the_referring_file() {
        assert_eq!(join_relative("a/b/main.rng", "sub/x.rng"), "a/b/sub/x.rng");
        assert_eq!(join_relative("a/b/main.rng", "../x.rng"), "a/x.rng");
        assert_eq!(join_relative("main.rng", "x.rng"), "x.rng");
        assert_eq!(join_relative("a/b/main.rng", "./x.rng"), "a/b/x.rng");
    }
}
