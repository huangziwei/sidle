//! NVDL — the dispatch layer that decides *which* schema validates *what*.

use std::collections::HashMap;

use super::relaxng::rng::{CompileError, join_relative};
use super::tree::{Builder, Document, NodeId, NodeKind};

/// The NVDL structure namespace.
pub const NVDL_NS: &str = "http://purl.oclc.org/dsdl/nvdl/ns/structure/1.0";

/// What a `<validate>` sends a section to. The schema language is decided by the
/// file's extension, which is how epubcheck's own `schemaType` attributes read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaRef(pub String);

/// One decomposed part of a document, and the schema it must satisfy.
#[derive(Debug)]
pub struct Section {
    pub schema: SchemaRef,
    pub document: Document,
}

/// Which node kinds a `<namespace>`/`<anyNamespace>` rule applies to.
#[derive(Debug, Clone, Copy)]
struct Applies {
    elements: bool,
    attributes: bool,
}

impl Applies {
    /// `match` defaults to `"elements"` (spec §6.4.6); every rule in epubcheck's
    /// scripts that wants attributes says so.
    fn parse(value: Option<&str>) -> Applies {
        let value = value.unwrap_or("elements");
        Applies {
            elements: value.split_whitespace().any(|w| w == "elements"),
            attributes: value.split_whitespace().any(|w| w == "attributes"),
        }
    }
}

/// How an action continues into the matched element's content: a named mode,
/// or the current one when unstated, with per-path overrides.
#[derive(Debug, Clone, Default)]
struct ModeUsage {
    use_mode: Option<String>,
    /// `<context path="…" useMode="…"/>` — inside an element whose name matches
    /// `path`, the mode is this one instead.
    contexts: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
enum Action {
    /// Start (or continue) a section validated against `schema`.
    Validate { schema: String, usage: ModeUsage },
    /// Keep the element in the enclosing section.
    Attach(ModeUsage),
    /// Drop the element but keep processing its content in the enclosing
    /// section — what `<switch>` does to its XHTML and SVG children.
    Unwrap(ModeUsage),
    /// Drop the element and everything under it; no error.
    Allow(ModeUsage),
    /// An error: this content may not appear here at all.
    Reject,
}

impl Action {
    fn usage(&self) -> Option<&ModeUsage> {
        match self {
            Action::Validate { usage, .. }
            | Action::Attach(usage)
            | Action::Unwrap(usage)
            | Action::Allow(usage) => Some(usage),
            Action::Reject => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Rule {
    /// `None` for `<anyNamespace>`, which is the fallback.
    namespace: Option<String>,
    applies: Applies,
    actions: Vec<Action>,
}

#[derive(Debug, Default)]
struct Mode {
    rules: Vec<Rule>,
}

impl Mode {
    /// The rule for `namespace`, preferring an exact `<namespace ns="…">` over
    /// the `<anyNamespace>` fallback, as the specification requires.
    fn rule(&self, namespace: Option<&str>, element: bool) -> Option<&Rule> {
        let wanted = namespace.unwrap_or("");
        let applies = |r: &&Rule| {
            if element {
                r.applies.elements
            } else {
                r.applies.attributes
            }
        };
        self.rules
            .iter()
            .find(|r| r.namespace.as_deref() == Some(wanted))
            .filter(applies)
            .or_else(|| {
                self.rules
                    .iter()
                    .filter(|r| r.namespace.is_none())
                    .find(applies)
            })
    }
}

/// A compiled NVDL script.
#[derive(Debug)]
pub struct Rules {
    /// The path the script was read from, so a `schema` href resolves.
    base: String,
    start_mode: String,
    modes: HashMap<String, Mode>,
}

impl Rules {
    /// Compile the NVDL script in `source`, whose path is `base`.
    pub fn compile(base: &str, source: &str) -> Result<Rules, CompileError> {
        let doc = Document::parse(source)
            .map_err(|e| CompileError(format!("{base}: not well-formed: {e}")))?;
        let root = doc
            .root_element()
            .ok_or_else(|| CompileError(format!("{base}: empty script")))?;
        if local_name(&doc, root) != Some("rules") {
            return Err(CompileError(format!("{base}: root is not <rules>")));
        }
        let mut modes = HashMap::new();
        for node in doc.element_children(root) {
            match local_name(&doc, node) {
                Some("mode") => {
                    let name = doc
                        .attr(node, None, "name")
                        .ok_or_else(|| CompileError(format!("{base}: <mode> has no name")))?
                        .to_string();
                    modes.insert(name, compile_mode(&doc, node, base)?);
                }
                // `<namespace>`/`<anyNamespace>` directly under `<rules>` is the
                // abbreviated form: the whole script is one anonymous mode.
                Some("namespace") | Some("anyNamespace") => {}
                // `<trigger>` affects section grouping only, which this does not
                // model; anything else is unknown and must not pass silently.
                Some("trigger") => {}
                other => {
                    return Err(CompileError(format!(
                        "{base}: <{}> is not <rules> content",
                        other.unwrap_or("?")
                    )));
                }
            }
        }
        // The abbreviated form has no `startMode`; its rules are `<rules>`' own.
        let start_mode = match doc.attr(root, None, "startMode") {
            Some(name) => name.to_string(),
            None => {
                modes.insert(String::new(), compile_mode(&doc, root, base)?);
                String::new()
            }
        };
        if !modes.contains_key(&start_mode) {
            return Err(CompileError(format!(
                "{base}: startMode {start_mode:?} is not defined"
            )));
        }
        Ok(Rules {
            base: base.to_string(),
            start_mode,
            modes,
        })
    }

    /// Decompose `doc` into the sections its schemas validate, plus the elements
    /// rejected outright.
    pub fn dispatch(&self, doc: &Document) -> Dispatch {
        let mut dispatch = Dispatch {
            sections: Vec::new(),
            rejected: Vec::new(),
        };
        let mut state = Walk {
            open: Vec::new(),
            out: &mut dispatch,
        };
        for child in doc.element_children(doc.root()) {
            let frame = Frame {
                mode: &self.start_mode,
                contexts: &[],
                section: None,
                section_ns: None,
            };
            self.walk(doc, child, &frame, &mut state);
        }
        for section in state.open {
            dispatch.sections.push(Section {
                schema: section.schema,
                document: section.builder.finish(),
            });
        }
        dispatch
    }

    /// Process one element.
    fn walk(&self, doc: &Document, node: NodeId, frame: &Frame, state: &mut Walk) {
        let Some(element) = doc.element(node) else {
            return;
        };
        let namespace = doc.expanded(&element.name).0.map(str::to_string);

        if let (Some(index), Some(section_ns)) = (frame.section, frame.section_ns.as_ref())
            && *section_ns == namespace
        {
            let parent = *state.open[index]
                .cursor
                .last()
                .expect("a section always has its root on the cursor");
            self.copy(doc, node, frame, index, parent, state);
            return;
        }

        let Some(mode_rules) = self.modes.get(frame.mode) else {
            return;
        };
        let Some(rule) = mode_rules.rule(namespace.as_deref(), true) else {
            return;
        };

        for action in &rule.actions {
            let usage = action.usage();
            let next_mode = usage
                .and_then(|u| u.use_mode.as_deref())
                .unwrap_or(frame.mode);
            // A mode usage without contexts of its own keeps the ones already in
            // force, so the `<context path="title">` on the outermost
            // `<validate>` still governs a `<title>` several levels down.
            let next_contexts: &[(String, String)] = match usage {
                Some(u) if !u.contexts.is_empty() => &u.contexts,
                _ => frame.contexts,
            };
            match action {
                Action::Reject => state.out.rejected.push(node),
                Action::Validate { schema, .. } => {
                    let schema = SchemaRef(join_relative(&self.base, schema));
                    // One section per schema per enclosing section, so sibling
                    // islands going to the same schema land in one document.
                    let index = match state
                        .open
                        .iter()
                        .position(|s| s.schema == schema && s.parent == frame.section)
                    {
                        Some(i) => i,
                        None => {
                            state.open.push(OpenSection {
                                schema,
                                parent: frame.section,
                                builder: Builder::new(doc.line_map()),
                                cursor: Vec::new(),
                            });
                            state.open.len() - 1
                        }
                    };
                    let root = state.open[index].builder.root();
                    state.open[index].cursor.push(root);
                    let inner = Frame {
                        mode: next_mode,
                        contexts: next_contexts,
                        section: Some(index),
                        section_ns: Some(namespace.clone()),
                    };
                    self.copy(doc, node, &inner, index, root, state);
                    state.open[index].cursor.pop();
                }
                Action::Attach(_) => {
                    let Some(index) = frame.section else { continue };
                    let parent = *state.open[index]
                        .cursor
                        .last()
                        .expect("a section always has its root on the cursor");
                    let inner = Frame {
                        mode: next_mode,
                        contexts: next_contexts,
                        section: Some(index),
                        section_ns: Some(namespace.clone()),
                    };
                    self.copy(doc, node, &inner, index, parent, state);
                }
                // Neither keeps the element itself. Its children are dispatched
                Action::Unwrap(_) | Action::Allow(_) => {
                    let inner_mode =
                        context_mode(next_contexts, &element.name.local).unwrap_or(next_mode);
                    let inner = Frame {
                        mode: inner_mode,
                        contexts: next_contexts,
                        section: frame.section,
                        section_ns: None,
                    };
                    for child in doc.element_children(node) {
                        self.walk(doc, child, &inner, state);
                    }
                }
            }
        }
    }

    /// Copy `node` into section `index` under `parent`, then walk its content.
    fn copy(
        &self,
        doc: &Document,
        node: NodeId,
        frame: &Frame,
        index: usize,
        parent: NodeId,
        state: &mut Walk,
    ) {
        let element = doc.element(node).expect("caller checked");
        let namespace = doc.expanded(&element.name).0.map(str::to_string);
        // Attributes are dispatched by their own namespace in the same mode. One
        let mode_rules = self.modes.get(frame.mode);
        let attrs: Vec<(Option<String>, String, String)> = element
            .attrs
            .iter()
            .filter(|a| {
                let ns = doc.expanded(&a.name).0;
                match mode_rules.and_then(|m| m.rule(ns, false)) {
                    None => true,
                    Some(rule) => rule
                        .actions
                        .iter()
                        .any(|a| matches!(a, Action::Attach(_) | Action::Validate { .. })),
                }
            })
            .map(|a| {
                (
                    doc.expanded(&a.name).0.map(str::to_string),
                    a.name.local.clone(),
                    a.value.clone(),
                )
            })
            .collect();

        let copied = state.open[index].builder.push_element(
            parent,
            namespace.as_deref(),
            &element.name.local,
            &attrs,
            element.prefix.clone(),
            doc.node(node).offset,
        );

        // A `<context path="x">` swaps the mode for the content of every element
        // named `x` inside the section.
        let inner_mode = context_mode(frame.contexts, &element.name.local).unwrap_or(frame.mode);
        let inner = Frame {
            mode: inner_mode,
            contexts: frame.contexts,
            section: Some(index),
            section_ns: Some(namespace),
        };

        state.open[index].cursor.push(copied);
        for child in doc.children(node) {
            match &doc.node(child).kind {
                NodeKind::Text(text) => {
                    state.open[index].builder.push_text(
                        copied,
                        text.clone(),
                        doc.node(child).offset,
                    );
                }
                NodeKind::Element(_) => self.walk(doc, child, &inner, state),
                NodeKind::Document => {}
            }
        }
        state.open[index].cursor.pop();
    }
}

/// What governs one element's processing.
struct Frame<'a> {
    mode: &'a str,
    contexts: &'a [(String, String)],
    /// The section the element's parent belongs to, if any.
    section: Option<usize>,
    /// That parent's namespace. `None` forces a dispatch, which is how an
    /// element whose parent was unwrapped or allowed starts over.
    section_ns: Option<Option<String>>,
}

/// The sections open during one dispatch.
struct Walk<'a> {
    open: Vec<OpenSection>,
    out: &'a mut Dispatch,
}

fn context_mode<'a>(contexts: &'a [(String, String)], local: &str) -> Option<&'a str> {
    contexts
        .iter()
        .find(|(path, _)| path_matches(path, local))
        .map(|(_, mode)| mode.as_str())
}

/// The result of decomposing a document.
#[derive(Debug)]
pub struct Dispatch {
    pub sections: Vec<Section>,
    /// Elements a mode rejected outright — content that may not appear where it
    /// does, independently of any grammar.
    pub rejected: Vec<NodeId>,
}

/// A section still being built.
struct OpenSection {
    schema: SchemaRef,
    /// The section this one is nested in, so sibling islands of the same schema
    /// under different parents stay apart.
    parent: Option<usize>,
    builder: Builder,
    /// The stack of copied ancestors, so an `attach` knows where to append.
    cursor: Vec<NodeId>,
}

/// NVDL paths are `/`-separated element names; only the last step is compared,
/// because every path epubcheck writes is a single name.
fn path_matches(path: &str, local: &str) -> bool {
    path.split('|')
        .map(str::trim)
        .any(|p| p.rsplit('/').next() == Some(local))
}

fn compile_mode(doc: &Document, node: NodeId, base: &str) -> Result<Mode, CompileError> {
    let mut mode = Mode::default();
    for child in doc.element_children(node) {
        let namespace = match local_name(doc, child) {
            Some("namespace") => Some(
                doc.attr(child, None, "ns")
                    .ok_or_else(|| CompileError(format!("{base}: <namespace> has no ns")))?
                    .to_string(),
            ),
            Some("anyNamespace") => None,
            other => {
                return Err(CompileError(format!(
                    "{base}: <{}> is not <mode> content",
                    other.unwrap_or("?")
                )));
            }
        };
        let mut actions = Vec::new();
        for action in doc.element_children(child) {
            let usage = compile_usage(doc, action);
            actions.push(match local_name(doc, action) {
                Some("validate") => Action::Validate {
                    schema: doc
                        .attr(action, None, "schema")
                        .ok_or_else(|| CompileError(format!("{base}: <validate> has no schema")))?
                        .to_string(),
                    usage,
                },
                Some("attach") => Action::Attach(usage),
                Some("unwrap") => Action::Unwrap(usage),
                Some("allow") => Action::Allow(usage),
                Some("reject") => Action::Reject,
                other => {
                    return Err(CompileError(format!(
                        "{base}: <{}> is not an NVDL action",
                        other.unwrap_or("?")
                    )));
                }
            });
        }
        mode.rules.push(Rule {
            namespace,
            applies: Applies::parse(doc.attr(child, None, "match")),
            actions,
        });
    }
    Ok(mode)
}

fn compile_usage(doc: &Document, node: NodeId) -> ModeUsage {
    ModeUsage {
        use_mode: doc.attr(node, None, "useMode").map(str::to_string),
        contexts: doc
            .element_children(node)
            .filter(|c| local_name(doc, *c) == Some("context"))
            .filter_map(|c| {
                Some((
                    doc.attr(c, None, "path")?.to_string(),
                    doc.attr(c, None, "useMode")?.to_string(),
                ))
            })
            .collect(),
    }
}

/// The local name of `node` if it is in the NVDL namespace.
fn local_name(doc: &Document, node: NodeId) -> Option<&str> {
    let element = doc.element(node)?;
    let (ns, local) = doc.expanded(&element.name);
    (ns == Some(NVDL_NS)).then_some(local)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::source::epub::xml::schema;

    fn rules(path: &str) -> Rules {
        Rules::compile(path, schema::get(path).expect("vendored"))
            .unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    /// Every dispatch script epubcheck names has to compile, for the same reason
    /// every grammar does: a script that does not would silently stop a whole
    /// document type from being checked.
    #[test]
    fn every_dispatch_script_compiles() {
        for path in [
            "20/rng/ops20.nvdl",
            "20/rng/ops20-svg.nvdl",
            "30/epub-xhtml-30.nvdl",
            "30/epub-svg-30.nvdl",
            "30/epub-svg-30-informative.nvdl",
            "30/epub-nav-30.nvdl",
            "30/media-overlay-30.nvdl",
            "30/ocf-container-30.nvdl",
            "30/ocf-metadata-30.nvdl",
            "30/package-30.nvdl",
        ] {
            let compiled = rules(path);
            assert!(!compiled.modes.is_empty(), "{path} defines no modes");
        }
    }

    fn serialize(doc: &Document, node: NodeId, out: &mut String) {
        match &doc.node(node).kind {
            NodeKind::Document => {
                for c in doc.children(node) {
                    serialize(doc, c, out);
                }
            }
            NodeKind::Text(t) => out.push_str(t),
            NodeKind::Element(e) => {
                out.push('<');
                out.push_str(&e.name.local);
                for a in &e.attrs {
                    out.push(' ');
                    out.push_str(&a.name.local);
                    out.push_str("=\"");
                    out.push_str(&a.value);
                    out.push('"');
                }
                out.push('>');
                for c in doc.children(node) {
                    serialize(doc, c, out);
                }
                out.push_str("</");
                out.push_str(&e.name.local);
                out.push('>');
            }
        }
    }

    fn section_text(section: &Section) -> String {
        let mut out = String::new();
        serialize(&section.document, section.document.root(), &mut out);
        out
    }

    #[test]
    fn xhtml_keeps_its_svg_islands_in_one_section() {
        // `30/epub-xhtml-30.nvdl` attaches every namespace, so the section handed
        // to the grammar is the whole document — which is why the XHTML grammar
        // has to include SVG in the first place.
        let dispatch = rules("30/epub-xhtml-30.nvdl").dispatch(
            &Document::parse(
                r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>
                <p>x</p><svg xmlns="http://www.w3.org/2000/svg"><rect/></svg>
                </body></html>"#,
            )
            .expect("well-formed"),
        );
        let schemas: Vec<&SchemaRef> = dispatch.sections.iter().map(|s| &s.schema).collect();
        assert_eq!(
            schemas,
            [
                &SchemaRef("30/epub-xhtml-30.rnc".into()),
                &SchemaRef("30/epub-xhtml-30.sch".into())
            ],
            "a grammar and a Schematron set, each with its own section"
        );
        for section in &dispatch.sections {
            let text = section_text(section);
            assert!(
                text.contains("<rect>"),
                "the SVG island is attached: {text}"
            );
        }
    }

    #[test]
    fn foreign_content_in_svg_is_dropped_not_rejected() {
        // `allowForeignNS` says `allow` for any other namespace, so the grammar
        // never sees it — reporting it would be a false RSC-005.
        let dispatch = rules("30/epub-svg-30.nvdl").dispatch(
            &Document::parse(
                r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:z="urn:z" z:a="1">
                <rect/><z:thing><rect/></z:thing></svg>"#,
            )
            .expect("well-formed"),
        );
        let text = section_text(&dispatch.sections[0]);
        assert!(text.contains("<rect>"), "SVG survives: {text}");
        assert!(
            !text.contains("thing"),
            "the foreign element is gone: {text}"
        );
        assert!(!text.contains("a=\"1\""), "so is its attribute: {text}");
        assert!(dispatch.rejected.is_empty(), "dropped, not an error");
    }

    #[test]
    fn foreign_elements_in_a_title_are_rejected() {
        // `allowOnlyHTML` is the one mode with a `reject`, reached through the
        // `<context path="title">` on the XHTML `<validate>`.
        let dispatch = rules("30/epub-xhtml-30.nvdl").dispatch(
            &Document::parse(
                r#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:z="urn:z"><head>
                <title>t<z:thing/></title></head><body><p><z:thing/></p></body></html>"#,
            )
            .expect("well-formed"),
        );
        assert_eq!(
            dispatch.rejected.len(),
            1,
            "only the one inside <title> is rejected"
        );
    }

    #[test]
    fn an_epub2_switch_reaches_two_schemas_at_once() {
        // The `<switch>` rule both unwraps its XHTML/SVG content into the
        // enclosing document and validates the switch itself against ops20.rng.
        let dispatch = rules("20/rng/ops20.nvdl").dispatch(
            &Document::parse(
                r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>
                <ops:switch xmlns:ops="http://www.idpf.org/2007/ops">
                  <ops:case required-namespace="urn:x"><p>a</p></ops:case>
                  <ops:default><p>b</p></ops:default>
                </ops:switch></body></html>"#,
            )
            .expect("well-formed"),
        );
        let schemas: Vec<&SchemaRef> = dispatch.sections.iter().map(|s| &s.schema).collect();
        assert!(
            schemas.contains(&&SchemaRef("20/rng/content-xhtml.rng".into())),
            "the XHTML grammar: {schemas:?}"
        );
        assert!(
            schemas.contains(&&SchemaRef("20/rng/ops20.rng".into())),
            "and the switch grammar: {schemas:?}"
        );
        // The `<default>` clause's XHTML is unwrapped into the XHTML section, so
        // it is checked in its real context rather than as a loose fragment,
        // while the `<case>` clause — meant for a different renderer — is not.
        let xhtml = dispatch
            .sections
            .iter()
            .find(|s| s.schema == SchemaRef("20/rng/content-xhtml.rng".into()))
            .expect("an XHTML section");
        let text = section_text(xhtml);
        assert!(text.contains("<p>b</p>"), "the default clause: {text}");
        assert!(
            !text.contains("<p>a</p>"),
            "but not the case clause: {text}"
        );
        assert!(!text.contains("switch"), "nor the switch itself: {text}");

        // The switch's own grammar sees the switch and its clauses, with the
        // XHTML inside them removed.
        let ops = dispatch
            .sections
            .iter()
            .find(|s| s.schema == SchemaRef("20/rng/ops20.rng".into()))
            .expect("an OPS section");
        let text = section_text(ops);
        assert!(text.contains("<case"), "the clauses: {text}");
        assert!(!text.contains("<p>"), "without their content: {text}");
    }
}
