//! The assertion half of the schema engine.
//!
//! RELAX NG says what may appear where; it cannot say that an `@idref` resolves,
//! that a `dcterms:modified` occurs exactly once, or that an `<a>` has no `<a>`
//! inside it. epubcheck expresses those in **Schematron** — patterns of rules,
//! each a context expression plus assertions — and reports every failure through
//! the same `RSC-005` channel as a grammar violation. Roughly a third of the
//! `.sch` corpus states things no grammar could.
//!
//! Both Schematron namespaces the vendored files use are accepted: the ISO one
//! (`purl.oclc.org`), and the older `ascc.net` one the EPUB 2 schemas are still
//! written in.
//!
//! An expression this port cannot evaluate is dropped along with its rule rather
//! than guessed at — see [`xpath`]. That trades recall for the guarantee that
//! every finding is one epubcheck would also make.

pub mod xpath;

use std::collections::HashMap;
use std::rc::Rc;

use xpath::{Bindings, Context, Item, NodeRef, XPath, XPathError};

use crate::validate::source::epub::xml::tree::{Document, NodeId, NodeKind};

/// Supplies the text of a file an `<include>` names, resolved against the
/// including file's path. Returns the resolved path and its content.
pub type Resolver<'a> = &'a dyn Fn(&str, &str) -> Option<(String, String)>;

/// The ISO Schematron namespace.
pub const ISO_NS: &str = "http://purl.oclc.org/dsdl/schematron";
/// The namespace of the earlier draft, which the EPUB 2 schemas still use.
pub const ASCC_NS: &str = "http://www.ascc.net/xml/schematron";

/// How loudly a failed assertion speaks. epubcheck reads the message text: one
/// beginning `WARNING` is downgraded, everything else is an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// One assertion that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub node: NodeId,
    pub line: u32,
    pub severity: Severity,
    pub message: String,
}

/// What a compilation failed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError(pub String);

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<XPathError> for CompileError {
    fn from(e: XPathError) -> CompileError {
        CompileError(e.0)
    }
}

/// A `<value-of select>` interleaved with the literal text around it.
#[derive(Debug, Clone)]
enum MessagePart {
    Text(String),
    Value(XPath),
}

#[derive(Debug, Clone)]
struct Assertion {
    test: XPath,
    /// `<report>` fires when its test *holds*; `<assert>` when it does not.
    on_true: bool,
    message: Vec<MessagePart>,
}

#[derive(Debug, Clone)]
struct Rule {
    context: XPath,
    lets: Vec<(String, XPath)>,
    assertions: Vec<Assertion>,
}

#[derive(Debug, Clone, Default)]
struct Pattern {
    lets: Vec<(String, XPath)>,
    rules: Vec<Rule>,
}

/// A compiled Schematron schema.
#[derive(Debug)]
pub struct Schema {
    namespaces: HashMap<String, String>,
    lets: Vec<(String, XPath)>,
    patterns: Vec<Pattern>,
    /// Rules whose context or tests this port cannot evaluate, kept so a caller
    /// can see what recall it is giving up rather than discovering it silently.
    pub skipped: Vec<String>,
}

impl Schema {
    /// Compile the Schematron schema in `source`, following `<include>` through
    /// `resolve`.
    pub fn compile(base: &str, source: &str, resolve: Resolver) -> Result<Schema, CompileError> {
        let doc = Document::parse(source)
            .map_err(|e| CompileError(format!("{base}: not well-formed: {e}")))?;
        let body = schema_body(&doc)
            .ok_or_else(|| CompileError(format!("{base}: not a Schematron schema")))?;

        // Namespaces first: every expression is parsed against them, and an
        // `<ns>` may follow the pattern whose prefix it declares.
        let mut namespaces = HashMap::new();
        collect_namespaces(&doc, body, base, resolve, &mut namespaces);

        let mut schema = Schema {
            namespaces,
            lets: Vec::new(),
            patterns: Vec::new(),
            skipped: Vec::new(),
        };

        // Gather every document first. An `<include>` may bring in the abstract
        // pattern that an earlier `is-a` names, so nothing can be compiled until
        // the whole set is in hand.
        let mut docs = vec![(doc, body)];
        let mut index = 0;
        while index < docs.len() {
            let mut included = Vec::new();
            schema.includes(&docs[index].0, docs[index].1, base, resolve, &mut included);
            docs.extend(included);
            index += 1;
        }

        // Abstract patterns are templates instantiated by `<pattern is-a>`, so
        // they are collected as syntax rather than compiled where they stand.
        let mut abstracts: HashMap<String, (usize, NodeId)> = HashMap::new();
        for (index, (doc, body)) in docs.iter().enumerate() {
            for child in doc.element_children(*body) {
                if local_name(doc, child) == Some("pattern")
                    && doc.attr(child, None, "abstract") == Some("true")
                    && let Some(id) = doc.attr(child, None, "id")
                {
                    abstracts.insert(id.to_string(), (index, child));
                }
            }
        }

        for index in 0..docs.len() {
            schema.body(&docs, index, docs[index].1, &abstracts);
        }
        Ok(schema)
    }

    /// Collect the documents an `<include>` chain reaches.
    fn includes(
        &mut self,
        doc: &Document,
        node: NodeId,
        base: &str,
        resolve: Resolver,
        out: &mut Vec<(Document, NodeId)>,
    ) {
        for child in doc.element_children(node) {
            if local_name(doc, child) != Some("include") {
                continue;
            }
            let Some(href) = doc.attr(child, None, "href") else {
                continue;
            };
            let Some((path, source)) = resolve(base, href) else {
                self.skipped
                    .push(format!("cannot resolve include {href:?}"));
                continue;
            };
            // An include may name a whole `<schema>` or a bare `<pattern>`
            // fragment; both are spliced in, which is how `id-unique` is shared
            // between six of these schemas.
            match Document::parse(&source) {
                Ok(parsed) => match schema_body(&parsed) {
                    Some(body) => out.push((parsed, body)),
                    None => self.skipped.push(format!("{path}: not Schematron content")),
                },
                Err(e) => self.skipped.push(format!("{path}: {e}")),
            }
        }
    }

    /// Compile one schema document's body.
    fn body(
        &mut self,
        docs: &[(Document, NodeId)],
        index: usize,
        node: NodeId,
        abstracts: &HashMap<String, (usize, NodeId)>,
    ) {
        let doc = &docs[index].0;
        for child in doc.element_children(node) {
            match local_name(doc, child) {
                Some("let") => {
                    if let Some(binding) = self.let_binding(doc, child, &HashMap::new()) {
                        self.lets.push(binding);
                    }
                }
                // An abstract pattern defines nothing where it stands.
                Some("pattern") if doc.attr(child, None, "abstract") == Some("true") => {}
                Some("pattern") => {
                    let pattern = match doc.attr(child, None, "is-a") {
                        None => self.pattern(doc, child, &HashMap::new()),
                        Some(name) => {
                            let Some((template_doc, template)) = abstracts.get(name).copied()
                            else {
                                self.skipped
                                    .push(format!("pattern is-a {name:?} is not defined"));
                                continue;
                            };
                            let params: HashMap<String, String> = doc
                                .element_children(child)
                                .filter(|c| local_name(doc, *c) == Some("param"))
                                .filter_map(|c| {
                                    Some((
                                        doc.attr(c, None, "name")?.to_string(),
                                        doc.attr(c, None, "value")?.to_string(),
                                    ))
                                })
                                .collect();
                            self.pattern(&docs[template_doc].0, template, &params)
                        }
                    };
                    self.patterns.push(pattern);
                }
                // `<ns>` and `<include>` are read before anything is compiled;
                // `<title>`, `<p>` and `<diagnostics>` are documentation.
                _ => {}
            }
        }
    }

    fn pattern(
        &mut self,
        doc: &Document,
        node: NodeId,
        params: &HashMap<String, String>,
    ) -> Pattern {
        let mut pattern = Pattern::default();
        for child in doc.element_children(node) {
            match local_name(doc, child) {
                Some("let") => {
                    if let Some(binding) = self.let_binding(doc, child, params) {
                        pattern.lets.push(binding);
                    }
                }
                Some("rule") => {
                    if let Some(rule) = self.rule(doc, child, params) {
                        pattern.rules.push(rule);
                    }
                }
                _ => {}
            }
        }
        pattern
    }

    fn rule(
        &mut self,
        doc: &Document,
        node: NodeId,
        params: &HashMap<String, String>,
    ) -> Option<Rule> {
        let context_source = substitute(doc.attr(node, None, "context")?, params);
        let context = match XPath::parse(&context_source, &self.namespaces) {
            Ok(x) => x.into_match_pattern(),
            Err(e) => {
                self.skipped
                    .push(format!("context {context_source:?}: {e}"));
                return None;
            }
        };
        let mut lets = Vec::new();
        let mut assertions = Vec::new();
        for child in doc.element_children(node) {
            match local_name(doc, child) {
                Some("let") => {
                    // A `let` that will not compile leaves every expression
                    // referring to it undefined, so the whole rule goes rather
                    // than a silently weakened subset of its assertions.
                    lets.push(self.let_binding(doc, child, params)?);
                }
                Some(kind @ ("assert" | "report")) => {
                    let Some(test_source) = doc.attr(child, None, "test") else {
                        continue;
                    };
                    let test_source = substitute(test_source, params);
                    match XPath::parse(&test_source, &self.namespaces) {
                        Ok(test) => {
                            let message = self.message(doc, child, params);
                            assertions.push(Assertion {
                                test,
                                on_true: kind == "report",
                                message,
                            });
                        }
                        Err(e) => self.skipped.push(format!("test {test_source:?}: {e}")),
                    }
                }
                _ => {}
            }
        }
        Some(Rule {
            context,
            lets,
            assertions,
        })
    }

    /// The text of an `<assert>`/`<report>`, with its `<value-of>` holes.
    fn message(
        &mut self,
        doc: &Document,
        node: NodeId,
        params: &HashMap<String, String>,
    ) -> Vec<MessagePart> {
        let mut parts = Vec::new();
        for child in doc.children(node) {
            match &doc.node(child).kind {
                NodeKind::Text(t) => parts.push(MessagePart::Text(t.clone())),
                NodeKind::Element(_) => {
                    if local_name(doc, child) == Some("value-of")
                        && let Some(select) = doc.attr(child, None, "select")
                    {
                        let select = substitute(select, params);
                        match XPath::parse(&select, &self.namespaces) {
                            Ok(x) => parts.push(MessagePart::Value(x)),
                            // A message hole that will not compile costs the
                            // detail only, so the assertion itself stands.
                            Err(_) => parts.push(MessagePart::Text("?".into())),
                        }
                    } else {
                        let inner = self.message(doc, child, params);
                        parts.extend(inner);
                    }
                }
                NodeKind::Document => {}
            }
        }
        parts
    }

    fn let_binding(
        &mut self,
        doc: &Document,
        node: NodeId,
        params: &HashMap<String, String>,
    ) -> Option<(String, XPath)> {
        let (name, value) = (
            doc.attr(node, None, "name")?,
            doc.attr(node, None, "value")?,
        );
        let (name, value) = (name.to_string(), substitute(value, params));
        match XPath::parse(&value, &self.namespaces) {
            Ok(x) => Some((name, x)),
            Err(e) => {
                self.skipped.push(format!("let ${name} = {value:?}: {e}"));
                None
            }
        }
    }

    /// Run every pattern over `doc`.
    ///
    /// Schematron's firing rule: within a pattern each node is matched by the
    /// *first* rule whose context selects it, and by no other. Patterns are
    /// independent of one another, so the same node may fire a rule in each.
    pub fn validate(&self, doc: &Document) -> Vec<Violation> {
        let mut out = Vec::new();
        let all: Vec<NodeId> = doc.descendants(doc.root());

        // Global `<let>`s are evaluated once, against the document node.
        let mut globals: Bindings = HashMap::new();
        for (name, expr) in &self.lets {
            let mut ctx = Context::new(doc, NodeRef::element(doc.root()));
            ctx.vars = globals.clone();
            if let Ok(value) = expr.eval(&ctx) {
                globals.insert(name.clone(), Rc::new(value));
            }
        }

        for pattern in &self.patterns {
            let mut vars = globals.clone();
            for (name, expr) in &pattern.lets {
                let mut ctx = Context::new(doc, NodeRef::element(doc.root()));
                ctx.vars = vars.clone();
                if let Ok(value) = expr.eval(&ctx) {
                    vars.insert(name.clone(), Rc::new(value));
                }
            }

            // Each rule's context is evaluated once for the whole document and
            // the results intersected with what earlier rules already claimed —
            // far cheaper than re-testing every rule at every node, and it is
            // what "the first matching rule wins" means.
            //
            // Both memberships are sets, one slot per node: a chapter-sized
            // document has as many nodes as a rule can select, so testing them
            // by scanning made the walk quadratic in document size — 800 KB of
            // XHTML took eighteen minutes, all of it here.
            let mut claimed = vec![false; doc.len()];
            for rule in &pattern.rules {
                let mut ctx = Context::new(doc, NodeRef::element(doc.root()));
                ctx.vars = vars.clone();
                let Ok(selected) = rule.context.eval(&ctx) else {
                    continue;
                };
                let mut is_selected = vec![false; doc.len()];
                for item in &selected {
                    if let Item::Node(n) = item
                        && n.attr.is_none()
                    {
                        is_selected[n.node.index()] = true;
                    }
                }
                for node in all.iter().copied() {
                    let slot = node.index();
                    if !is_selected[slot] || claimed[slot] {
                        continue;
                    }
                    claimed[slot] = true;
                    self.fire(doc, rule, node, &vars, &mut out);
                }
            }
        }
        out
    }

    fn fire(
        &self,
        doc: &Document,
        rule: &Rule,
        node: NodeId,
        vars: &Bindings,
        out: &mut Vec<Violation>,
    ) {
        let mut ctx = Context::new(doc, NodeRef::element(node));
        ctx.vars = vars.clone();
        for (name, expr) in &rule.lets {
            match expr.eval(&ctx) {
                Ok(value) => {
                    ctx.vars.insert(name.clone(), Rc::new(value));
                }
                // A `let` that fails here leaves the assertions below unable to
                // mean anything; say nothing rather than something wrong.
                Err(_) => return,
            }
        }
        for assertion in &rule.assertions {
            let Ok(holds) = assertion.test.eval_bool(&ctx) else {
                continue;
            };
            if holds != assertion.on_true {
                continue;
            }
            let message: String = assertion
                .message
                .iter()
                .map(|part| match part {
                    MessagePart::Text(t) => t.clone(),
                    MessagePart::Value(x) => x.eval_string(&ctx).unwrap_or_default(),
                })
                .collect();
            let message = message.split_whitespace().collect::<Vec<_>>().join(" ");
            out.push(Violation {
                node,
                line: doc.line(node),
                severity: match message.starts_with("WARNING") {
                    true => Severity::Warning,
                    false => Severity::Error,
                },
                message,
            });
        }
    }
}

/// Gather every `<ns prefix uri>`, following includes, before any expression is
/// parsed.
fn collect_namespaces(
    doc: &Document,
    root: NodeId,
    base: &str,
    resolve: Resolver,
    out: &mut HashMap<String, String>,
) {
    for child in doc.element_children(root) {
        match local_name(doc, child) {
            Some("ns") => {
                if let (Some(prefix), Some(uri)) = (
                    doc.attr(child, None, "prefix"),
                    doc.attr(child, None, "uri"),
                ) {
                    out.insert(prefix.to_string(), uri.to_string());
                }
            }
            Some("include") => {
                if let Some(href) = doc.attr(child, None, "href")
                    && let Some((path, source)) = resolve(base, href)
                    && let Ok(included) = Document::parse(&source)
                    && let Some(body) = schema_body(&included)
                {
                    collect_namespaces(&included, body, &path, resolve, out);
                }
            }
            _ => {}
        }
    }
}

/// Substitute an abstract pattern's `<param>` values.
///
/// The specification really does define this as textual replacement of `$name`
/// inside the attribute, which is how a schema can write `@$idref-attr-name` and
/// mean an attribute whose name the instantiating pattern chooses.
fn substitute(source: &str, params: &HashMap<String, String>) -> String {
    if params.is_empty() || !source.contains('$') {
        return source.to_string();
    }
    // Longest name first, so `$id` cannot eat the start of `$idref`.
    let mut names: Vec<&String> = params.keys().collect();
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));
    let mut out = source.to_string();
    for name in names {
        out = out.replace(&format!("${name}"), &params[name]);
    }
    out
}

/// The node whose element children are a schema's body.
///
/// For a whole `<schema>` that is the root element itself; for an included
/// fragment — a bare `<pattern>` — it is the document node, so the fragment is
/// the single body item.
fn schema_body(doc: &Document) -> Option<NodeId> {
    let root = doc.root_element()?;
    match local_name(doc, root)? {
        "schema" => Some(root),
        "pattern" | "rule" | "let" | "ns" | "include" => Some(doc.root()),
        _ => None,
    }
}

/// The local name of `node` if it is in either Schematron namespace.
fn local_name(doc: &Document, node: NodeId) -> Option<&str> {
    let element = doc.element(node)?;
    let (ns, local) = doc.expanded(&element.name);
    matches!(ns, Some(ISO_NS) | Some(ASCC_NS)).then_some(local)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::source::epub::xml::schema;

    fn no_includes(_: &str, _: &str) -> Option<(String, String)> {
        None
    }

    /// Resolve an `<include>` against the vendored schemas.
    fn vendored(base: &str, href: &str) -> Option<(String, String)> {
        let path = crate::validate::source::epub::xml::relaxng::rng::join_relative(base, href);
        schema::get(&path).map(|c| (path, c.to_string()))
    }

    fn compile(path: &str) -> Schema {
        Schema::compile(path, schema::get(path).expect("vendored"), &vendored)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn messages(schema: &Schema, xml: &str) -> Vec<String> {
        let doc = Document::parse(xml).expect("well-formed test document");
        schema
            .validate(&doc)
            .into_iter()
            .map(|v| v.message)
            .collect()
    }

    /// Every Schematron set epubcheck names has to compile, and every rule in it
    /// has to be one this port can evaluate. A skipped rule is silently lost
    /// recall, which is the failure this engine exists to prevent.
    #[test]
    fn every_assertion_set_compiles_with_nothing_skipped() {
        let mut skipped = Vec::new();
        for path in [
            "20/sch/opf.sch",
            "20/sch/ncx.sch",
            "20/sch/xhtml.sch",
            "20/sch/id-unique.sch",
            "30/package-30.sch",
            "30/epub-xhtml-30.sch",
            "30/epub-nav-30.sch",
            "30/epub-svg-30.sch",
            "30/media-overlay-30.sch",
            "30/ocf-encryption-30.sch",
            "30/ocf-metadata-30.sch",
            "30/mod/html5/assertions.sch",
            "30/mod/id-unique.sch",
            "30/multiple-renditions/container.sch",
            "30/multiple-renditions/mapping.sch",
            "30/collection-do-30.sch",
            "30/collection-manifest-30.sch",
            "30/dict/dict-opf.sch",
            "30/dict/dict-xhtml.sch",
            "30/dict/dict-collection.sch",
            "30/idx/idx-xhtml.sch",
            "30/idx/idx-xhtml-index.sch",
            "30/idx/idx-collection.sch",
            "30/datanav/datanav-xhtml.sch",
            "30/previews/preview-collection.sch",
            "30/previews/preview-pub-opf.sch",
            "30/edupub/edu-opf.sch",
            "30/edupub/edu-structure.sch",
            "30/edupub/edu-semantics.sch",
            "30/edupub/edu-ocf-metadata.sch",
        ] {
            let compiled = compile(path);
            for reason in &compiled.skipped {
                skipped.push(format!("{path}: {reason}"));
            }
        }
        assert!(
            skipped.is_empty(),
            "{} rules skipped:\n{}",
            skipped.len(),
            skipped.join("\n")
        );
    }

    #[test]
    fn nested_anchors_are_reported() {
        let schema = compile("20/sch/xhtml.sch");
        let nested = r##"<html xmlns="http://www.w3.org/1999/xhtml"><body>
            <p><a href="#x"><a href="#y">t</a></a></p></body></html>"##;
        let found = messages(&schema, nested);
        assert_eq!(found.len(), 1, "one report, on the outer <a>: {found:?}");
        assert!(found[0].contains("nested"), "{found:?}");

        let flat = r##"<html xmlns="http://www.w3.org/1999/xhtml"><body>
            <p><a href="#x">t</a><a href="#y">u</a></p></body></html>"##;
        assert!(messages(&schema, flat).is_empty(), "siblings are fine");
    }

    #[test]
    fn duplicate_ids_are_reported() {
        let schema = compile("20/sch/id-unique.sch");
        let doubled = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>
            <p id="a"/><p id="a"/></body></html>"#;
        assert!(!messages(&schema, doubled).is_empty(), "a repeated id");
        let unique = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>
            <p id="a"/><p id="b"/></body></html>"#;
        assert!(messages(&schema, unique).is_empty());
    }

    const OPF3: &str = r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
        <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
          <dc:identifier id="uid">u</dc:identifier>
          <dc:title>T</dc:title>
          <dc:language>en</dc:language>
          <meta property="dcterms:modified">2020-01-01T00:00:00Z</meta>
        </metadata>
        <manifest>
          <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
        </manifest>
        <spine><itemref idref="nav"/></spine></package>"#;

    #[test]
    fn the_real_package_assertions_judge_a_package() {
        let schema = compile("30/package-30.sch");
        let found = messages(&schema, OPF3);
        assert!(found.is_empty(), "a valid EPUB 3 package: {found:?}");

        // Each of these is a rule no grammar could state.
        for (label, xml, expect) in [
            (
                "unique-identifier must resolve",
                OPF3.replace(r#"unique-identifier="uid""#, r#"unique-identifier="nope""#),
                "unique-identifier",
            ),
            (
                "dcterms:modified must occur exactly once",
                OPF3.replace(
                    r#"<meta property="dcterms:modified">2020-01-01T00:00:00Z</meta>"#,
                    "",
                ),
                "dcterms:modified",
            ),
            (
                "and must be a timestamp",
                OPF3.replace("2020-01-01T00:00:00Z", "2020"),
                "illegal syntax",
            ),
            (
                "a refines must point at something",
                OPF3.replace(
                    "<dc:title>T</dc:title>",
                    r##"<dc:title>T</dc:title><meta property="x" refines="#gone">y</meta>"##,
                ),
                "refines",
            ),
        ] {
            let found = messages(&schema, &xml);
            assert!(
                found.iter().any(|m| m.contains(expect)),
                "{label}: expected a message mentioning {expect:?}, got {found:?}"
            );
        }
    }

    #[test]
    fn an_abstract_pattern_is_instantiated_with_its_parameters() {
        // `$attr` is replaced textually, so one abstract rule can check `@for`,
        // `@headers` and `@list` alike.
        let schema = Schema::compile(
            "t.sch",
            r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
                 <ns prefix="h" uri="http://www.w3.org/1999/xhtml"/>
                 <pattern id="idref" abstract="true">
                   <rule context="h:*[@$attr]">
                     <assert test="//h:*[@id = current()/@$attr]">no target</assert>
                   </rule>
                 </pattern>
                 <pattern is-a="idref"><param name="attr" value="for"/></pattern>
               </schema>"#,
            &no_includes,
        )
        .expect("compiles");
        assert!(schema.skipped.is_empty(), "{:?}", schema.skipped);
        let bad = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>
            <label for="gone"/></body></html>"#;
        assert_eq!(messages(&schema, bad).len(), 1);
        let good = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>
            <label for="here"/><input id="here"/></body></html>"#;
        assert!(messages(&schema, good).is_empty());
    }

    #[test]
    fn only_the_first_matching_rule_in_a_pattern_fires() {
        let schema = Schema::compile(
            "t.sch",
            r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
                 <ns prefix="h" uri="http://www.w3.org/1999/xhtml"/>
                 <pattern>
                   <rule context="h:p[@id]"><report test="true()">first</report></rule>
                   <rule context="h:p"><report test="true()">second</report></rule>
                 </pattern>
                 <pattern>
                   <rule context="h:p"><report test="true()">other pattern</report></rule>
                 </pattern>
               </schema>"#,
            &no_includes,
        )
        .expect("compiles");
        let found = messages(
            &schema,
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><p id="a"/><p/></body></html>"#,
        );
        assert_eq!(
            found,
            ["first", "second", "other pattern", "other pattern"],
            "the id'd p takes the first rule only, and both patterns still run"
        );
    }

    #[test]
    fn a_warning_message_is_not_an_error() {
        let schema = Schema::compile(
            "t.sch",
            r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
                 <ns prefix="h" uri="http://www.w3.org/1999/xhtml"/>
                 <pattern>
                   <rule context="h:p">
                     <report test="true()">WARNING: something worth mentioning</report>
                     <report test="true()">a real problem</report>
                   </rule>
                 </pattern>
               </schema>"#,
            &no_includes,
        )
        .expect("compiles");
        let doc = Document::parse(r#"<html xmlns="http://www.w3.org/1999/xhtml"><p/></html>"#)
            .expect("well-formed");
        let severities: Vec<Severity> = schema.validate(&doc).iter().map(|v| v.severity).collect();
        assert_eq!(severities, [Severity::Warning, Severity::Error]);
    }
}
