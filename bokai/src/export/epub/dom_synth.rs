//! DOM-synthesis regime of the normalized EPUB export: chapter documents
//! built through the shared XHTML DOM ([`super::dom`]), plus the stylesheet
//! machinery for source-declared style programs.

use std::collections::{HashMap, HashSet};

use crate::model::{Chapter, NodeId as IrNodeId, Role};
use crate::style::{CssDecl, parse_inline_decl};

use super::dom::{self, Dom, LayoutHints};
use super::normalize::{InlineStyleEmit, LinkOutcome, SourceElements, SourceStyles};

/// Per-chapter emission inputs for the DOM-synthesis path.
pub struct ChapterEmit<'a> {
    /// `<title>` text — the section name (calibre: `push_book_part`'s
    /// `title` argument), NOT the deduped filename.
    pub title: &'a str,
    /// Book language for `<html xml:lang lang>` (skipped when empty).
    pub language: &'a str,
    /// Named-class / inline-style resolution built book-wide by
    /// `normalize_book` (same maps the string-synthesis path used).
    pub source_styles: &'a SourceStyles<'a>,
    /// Link resolution for `semantics.href` values.
    pub href_resolver: &'a dyn Fn(&str) -> LinkOutcome,
    /// Fixed-layout page pixel viewport → `<meta name="viewport">` in the
    /// head. `None` for reflowable documents.
    pub viewport: Option<(u32, u32)>,
    /// Whether to stamp `data-eid` from `semantics.source_element`.
    pub source_elements: SourceElements,
    /// The assets the source actually holds, when the importer can enumerate
    /// them. An `<img>` whose `src` is not in this set degrades to a `<span>`
    /// carrying its alt text — see `Builder::emit_image`.
    pub available_assets: Option<&'a HashSet<String>>,
}

/// Build, consolidate, and serialize one chapter document. Referenced
/// image paths are added to `assets`.
pub fn emit_chapter(ir: &Chapter, opts: &ChapterEmit<'_>, assets: &mut HashSet<String>) -> String {
    let (mut dom, html_id, head_id, body_id) = dom::new_book_part(opts.title);
    // `xml:lang` + `lang` on `<html>` from the book language (calibre:
    // `push_book_part`; calibre `set_doc_lang`).
    let lang = opts.language.trim();
    if !lang.is_empty() {
        dom.get_mut(html_id).set("xml:lang", lang);
        dom.get_mut(html_id).set("lang", lang);
    }
    // Stylesheet link — sibling filename, matching calibre.
    let link = dom.sub_element(head_id, "link");
    let l = dom.get_mut(link);
    l.set("rel", "stylesheet");
    l.set("type", "text/css");
    l.set("href", "style.css");
    // Fixed-layout page viewport, after the stylesheet link (calibre:
    // `emit_fxl_page` adds it to the already-linked head).
    if let Some((w, h)) = opts.viewport {
        let meta = dom.sub_element(head_id, "meta");
        let m = dom.get_mut(meta);
        m.set("name", "viewport");
        m.set("content", format!("width={w}, height={h}"));
    }

    let mut b = Builder {
        ir,
        dom,
        classes: HashMap::new(),
        styles: HashMap::new(),
        hints: HashMap::new(),
        opts,
        assets,
        used_epub_type: false,
    };
    for child in ir.children(crate::model::NodeId::ROOT) {
        b.walk(child, body_id);
    }

    let Builder {
        mut dom,
        mut classes,
        mut styles,
        mut hints,
        used_epub_type,
        ..
    } = b;

    // The prefix has to be bound where it is used, and a document that states
    // no semantics declares nothing.
    if used_epub_type {
        dom.get_mut(html_id)
            .set("xmlns:epub", "http://www.idpf.org/2007/ops");
    }

    // A document whose whole content is one container: that container IS the
    // page, so it becomes `<body>` rather than a box inside it. See
    // [`merge_sole_container_into_body`].
    merge_sole_container_into_body(&mut dom, body_id, &mut classes, &mut styles, &mut hints);

    // Same pass order as calibre's pipeline (`build_output`): links are
    dom::consolidate_part(&mut dom, &classes, &styles, &hints);
    dom::normalize_lists_dom(&mut dom);
    dom::replace_eol_with_br_dom(&mut dom);
    dom::finalize_attrs(&mut dom, &classes, &styles);
    anchor_page_plate_height(&mut dom, head_id, body_id);
    dom::chapter_document(&dom)
}

/// The chain of boxes from `<body>` down to the one element a document's
/// whole content leads to, or `None` when the document branches.
fn sole_content_path(dom: &Dom, body: dom::NodeId) -> Option<Vec<dom::NodeId>> {
    let mut path = Vec::new();
    let mut id = body;
    loop {
        let elem = dom.get(id);
        if elem.text.as_deref().is_some_and(|t| !t.trim().is_empty()) {
            return None;
        }
        let &[only] = &elem.children[..] else {
            return (!path.is_empty()).then_some(path);
        };
        if dom
            .get(only)
            .tail
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
        {
            return None;
        }
        path.push(only);
        id = only;
    }
}

/// Give a full-page plate's percentage height something to resolve against.
fn anchor_page_plate_height(dom: &mut Dom, head: dom::NodeId, body: dom::NodeId) {
    let Some(path) = sole_content_path(dom, body) else {
        return;
    };
    let Some(&plate) = path.last() else {
        return;
    };
    if !matches!(dom.get(plate).tag.as_str(), "img" | "svg" | "video") {
        return;
    }
    let mut plate_decl = inline_decl(dom, plate);
    if !plate_decl
        .get("height")
        .is_some_and(|h| h.ends_with('%') && h != "0%")
    {
        return;
    }
    plate_decl.set("max-width", "100%");
    let inline = plate_decl.to_inline();
    dom.get_mut(plate).set("style", inline);

    // Every box between `<body>` and the plate, exclusive: the plate keeps
    // the height it states, and `<body>` takes its own from the rule below.
    for &id in &path[..path.len() - 1] {
        let mut decl = inline_decl(dom, id);
        if decl.get("height").is_none() {
            decl.set("height", "100%");
            let inline = decl.to_inline();
            dom.get_mut(id).set("style", inline);
        }
    }

    let style = dom.sub_element(head, "style");
    let el = dom.get_mut(style);
    el.set("type", "text/css");
    el.text = Some("html, body { height: 100% }".to_string());
}

/// An element's `style` attribute as a declaration list, empty when it has
/// none.
fn inline_decl(dom: &Dom, id: dom::NodeId) -> CssDecl {
    dom.get(id)
        .get("style")
        .map(parse_inline_decl)
        .unwrap_or_default()
}

/// Adopt a document's sole top-level container as its `<body>`.
fn merge_sole_container_into_body(
    dom: &mut Dom,
    body_id: dom::NodeId,
    classes: &mut HashMap<dom::NodeId, Vec<String>>,
    styles: &mut HashMap<dom::NodeId, CssDecl>,
    hints: &mut LayoutHints,
) {
    let [only] = dom.get(body_id).children[..] else {
        return;
    };
    // Only a plain container is the page; anything else keeps its own tag.
    if !matches!(dom.get(only).tag.as_str(), "div" | "aside" | "figure") {
        return;
    }
    // Text directly on `<body>` would have nowhere to go.
    if dom.get(body_id).text.is_some() || dom.get(only).tail.is_some() {
        return;
    }
    dom.move_into(only, body_id);
    if let Some(c) = classes.remove(&only) {
        classes.entry(body_id).or_default().extend(c);
    }
    if let Some(s) = styles.remove(&only) {
        let slot = styles.entry(body_id).or_default();
        for (k, v) in s.items {
            slot.set(k, v);
        }
    }
    if let Some(h) = hints.remove(&only) {
        hints.entry(body_id).or_insert(h);
    }
}

struct Builder<'a, 'b> {
    ir: &'a Chapter,
    dom: Dom,
    classes: HashMap<dom::NodeId, Vec<String>>,
    styles: HashMap<dom::NodeId, CssDecl>,
    hints: LayoutHints,
    opts: &'a ChapterEmit<'b>,
    assets: &'a mut HashSet<String>,
    /// Whether any element took an `epub:type`, which is what decides if the
    /// document has to bind the prefix.
    used_epub_type: bool,
}

impl Builder<'_, '_> {
    /// Append IR text lxml-style: onto the parent's leading text when it has no
    /// element children yet, else onto the last child's tail.
    fn append_text(&mut self, parent: dom::NodeId, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.dom.get(parent).children.last().copied() {
            Some(last) => {
                let e = self.dom.get_mut(last);
                let mut t = e.tail.clone().unwrap_or_default();
                t.push_str(text);
                e.tail = Some(t);
            }
            None => {
                let e = self.dom.get_mut(parent);
                let mut t = e.text.clone().unwrap_or_default();
                t.push_str(text);
                e.text = Some(t);
            }
        }
    }

    /// Resolve the node's `semantics.class` to the emitted class name, if any.
    fn named_class(&self, id: IrNodeId) -> Option<String> {
        let name = self.ir.semantics.class(id)?;
        match self.opts.source_styles.named.get(name) {
            Some(Some(class)) => Some(class.clone()),
            _ => None,
        }
    }

    /// Block-channel class/style: named class + promoted inline class go to
    /// the pending class map; an unpromoted inline declaration goes to the
    /// pending style map. Applied by `finalize_attrs` after consolidation.
    fn attach_block_style(&mut self, id: IrNodeId, el: dom::NodeId) {
        if let Some(class) = self.named_class(id) {
            self.classes.entry(el).or_default().push(class);
        }
        if let Some(raw) = self.ir.semantics.style(id) {
            match self.opts.source_styles.inline.get(raw) {
                Some(InlineStyleEmit::Class(g)) => {
                    self.classes.entry(el).or_default().push(g.clone());
                }
                Some(InlineStyleEmit::Style(s)) => {
                    self.styles.insert(el, parse_inline_decl(s));
                }
                Some(InlineStyleEmit::Drop) | None => {}
            }
        }
    }

    /// Inline-channel class: a real `class` attribute at creation time
    /// (calibre `attach_inline_style`), so `strip_empty_spans` keeps the
    /// element and a later `id` lands after it.
    fn attach_inline_class(&mut self, id: IrNodeId, el: dom::NodeId) {
        if let Some(class) = self.named_class(id) {
            self.dom.get_mut(el).set("class", class);
        }
    }

    fn stamp_id(&mut self, id: IrNodeId, el: dom::NodeId) {
        if let Some(elem_id) = self.ir.semantics.id(id)
            && !self.dom.get(el).has_attr("id")
        {
            self.dom.get_mut(el).set("id", elem_id);
        }
    }

    /// Carry the node's `epub:type` onto its element. It is what a reader
    fn stamp_epub_type(&mut self, id: IrNodeId, el: dom::NodeId) {
        let Some(epub_type) = self.ir.semantics.epub_type(id).map(str::to_string) else {
            return;
        };
        self.dom.get_mut(el).set("epub:type", epub_type);
        self.used_epub_type = true;
    }

    /// Emit an image, or the degradation for one whose bytes aren't there.
    fn emit_image(&mut self, id: IrNodeId, parent: dom::NodeId) {
        let src = self.ir.semantics.src(id);
        // `alt` is always present (calibre: calibre defaults "").
        let alt = self.ir.semantics.alt(id).unwrap_or("").to_string();
        let missing = match (src, self.opts.available_assets) {
            (Some(s), Some(have)) => !have.contains(s),
            // No `src` at all is equally unrenderable as an `<img>`.
            (None, _) => true,
            _ => false,
        };
        if missing {
            let span = self.dom.sub_element(parent, "span");
            if !alt.is_empty() {
                self.dom.get_mut(span).text = Some(alt);
            }
            self.stamp_source_element(id, span);
            self.stamp_id(id, span);
            self.stamp_epub_type(id, span);
            return;
        }
        let img = self.dom.sub_element(parent, "img");
        if let Some(src) = src {
            self.dom.get_mut(img).set("src", src);
            self.assets.insert(src.to_string());
        }
        self.dom.get_mut(img).set("alt", &alt);
        self.stamp_source_element(id, img);
        self.stamp_id(id, img);
        self.stamp_epub_type(id, img);
        self.attach_block_style(id, img);
    }

    /// Carry the node's source element id onto its DOM element, so a renderer
    fn stamp_source_element(&mut self, id: IrNodeId, el: dom::NodeId) {
        if self.opts.source_elements == SourceElements::Omit {
            return;
        }
        if let Some(eid) = self.ir.semantics.source_element(id) {
            self.dom.get_mut(el).set("data-eid", eid.to_string());
        }
    }

    fn walk(&mut self, id: IrNodeId, parent: dom::NodeId) {
        let Some(node) = self.ir.node(id) else {
            return;
        };
        let role = node.role;

        // Leaf text runs merge into the surrounding text slots.
        if role == Role::Text {
            let text = self.ir.text(node.text).to_string();
            self.append_text(parent, &text);
            return;
        }

        match role {
            Role::Ruby => {
                let ruby = self.dom.sub_element(parent, "ruby");
                self.attach_inline_class(id, ruby);
                self.stamp_source_element(id, ruby);
                self.stamp_id(id, ruby);
                self.stamp_epub_type(id, ruby);
                // Base content wraps in `<rb>`; the annotation children
                // (`RubyText`) follow as `<rt>` — calibre's rb/rt shape.
                let children: Vec<IrNodeId> = self.ir.children(id).collect();
                let mut rb: Option<dom::NodeId> = None;
                for child in children {
                    let is_rt = self
                        .ir
                        .node(child)
                        .is_some_and(|n| n.role == Role::RubyText);
                    if is_rt {
                        rb = None;
                        let rt = self.dom.sub_element(ruby, "rt");
                        // `<rt>` always exists, even with an empty
                        // annotation (calibre: lookup miss → "").
                        if self.dom.get(rt).text.is_none() {
                            self.dom.get_mut(rt).text = Some(String::new());
                        }
                        for rt_child in self.ir.children(child).collect::<Vec<_>>() {
                            self.walk(rt_child, rt);
                        }
                    } else {
                        let rb_el = *rb.get_or_insert_with(|| {
                            let el = self.dom.create_element("rb");
                            self.dom.append(ruby, el);
                            el
                        });
                        self.walk(child, rb_el);
                    }
                }
            }
            Role::RubyText => {
                // Standalone rt (outside a Ruby parent) — keep the tag.
                let rt = self.dom.sub_element(parent, "rt");
                for child in self.ir.children(id).collect::<Vec<_>>() {
                    self.walk(child, rt);
                }
            }
            Role::Link => {
                let a = self.dom.sub_element(parent, "a");
                if let Some(href) = self.ir.semantics.href(id) {
                    match (self.opts.href_resolver)(href) {
                        LinkOutcome::Keep => self.dom.get_mut(a).set("href", href),
                        LinkOutcome::Rewrite(new_href) => self.dom.get_mut(a).set("href", new_href),
                        LinkOutcome::DropHref => {}
                    }
                }
                self.attach_inline_class(id, a);
                self.stamp_source_element(id, a);
                self.stamp_id(id, a);
                self.stamp_epub_type(id, a);
                for child in self.ir.children(id).collect::<Vec<_>>() {
                    self.walk(child, a);
                }
            }
            Role::Inline => {
                let span = self.dom.sub_element(parent, "span");
                if self.ir.semantics.render_inline(id) {
                    // A demoted `render: inline` block keeps the block
                    self.stamp_source_element(id, span);
                    self.stamp_id(id, span);
                    self.stamp_epub_type(id, span);
                    self.attach_block_style(id, span);
                } else {
                    self.attach_inline_class(id, span);
                    self.stamp_source_element(id, span);
                    self.stamp_id(id, span);
                    self.stamp_epub_type(id, span);
                }
                for child in self.ir.children(id).collect::<Vec<_>>() {
                    self.walk(child, span);
                }
            }
            Role::Image => self.emit_image(id, parent),
            Role::Break => {
                self.dom.sub_element(parent, "br");
            }
            _ => {
                let mut tag = self.block_tag(role);
                // Row children that would emit as `<div>` become `<td>` —
                // calibre's rule (`emit_table_row`).
                if tag == "div" && self.dom.get(parent).tag == "tr" {
                    tag = "td";
                }
                let el = self.dom.sub_element(parent, tag);
                self.stamp_source_element(id, el);
                self.stamp_id(id, el);
                self.stamp_epub_type(id, el);
                self.attach_block_style(id, el);
                // A spanning cell keeps its geometry: dropping the attribute
                match tag {
                    "td" | "th" => {
                        if let Some(n) = self.ir.semantics.col_span(id) {
                            self.dom.get_mut(el).set("colspan", n.to_string());
                        }
                        if let Some(n) = self.ir.semantics.row_span(id) {
                            self.dom.get_mut(el).set("rowspan", n.to_string());
                        }
                    }
                    "col" | "colgroup" => {
                        if let Some(n) = self.ir.semantics.col_span(id) {
                            self.dom.get_mut(el).set("span", n.to_string());
                        }
                    }
                    // A numbered list interrupted by prose resumes at a stated
                    // ordinal; without it every fragment restarts at one.
                    "ol" => {
                        if let Some(n) = self.ir.semantics.list_start(id) {
                            self.dom.get_mut(el).set("start", n.to_string());
                        }
                    }
                    "li" => {
                        if let Some(n) = self.ir.semantics.list_start(id) {
                            self.dom.get_mut(el).set("value", n.to_string());
                        }
                    }
                    _ => {}
                }
                match role {
                    Role::Heading(level) => {
                        self.hints
                            .insert(el, (vec!["heading".to_string()], Some(level.to_string())));
                    }
                    Role::Figure => {
                        self.hints.insert(el, (vec!["figure".to_string()], None));
                    }
                    Role::Caption => {
                        self.hints.insert(el, (vec!["caption".to_string()], None));
                    }
                    _ => {}
                }
                for child in self.ir.children(id).collect::<Vec<_>>() {
                    self.walk(child, el);
                }
            }
        }
    }

    /// Tag for a block-level role, pre-consolidation. KFX block containers
    /// (including headings, figures, captions, quotes, and note asides — the
    /// calibre's EPUB-2.0-gated promotions) all emit `<div>`;
    fn block_tag(&self, role: Role) -> &'static str {
        match role {
            Role::OrderedList => "ol",
            Role::UnorderedList => "ul",
            Role::ListItem => "li",
            Role::Table => "table",
            Role::ColumnGroup => "colgroup",
            Role::Column => "col",
            Role::TableHead => "thead",
            Role::TableBody => "tbody",
            Role::TableRow => "tr",
            // Calibre never emits `<th>` (it converts row
            // children to `<td>`).
            Role::TableCell => "td",
            Role::Rule => "hr",
            // Only reachable from non-KFX sources; keep natural tags.
            Role::DefinitionList => "dl",
            Role::DefinitionTerm => "dt",
            Role::DefinitionDescription => "dd",
            Role::CodeBlock => "pre",
            _ => "div",
        }
    }
}
/// Sanitize a source style name into a valid CSS class name (and matching HTML
pub fn safe_class_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let needs_prefix = match out.as_bytes() {
        [] => true,
        [b'-'] => true,
        [b'-', d, ..] if d.is_ascii_digit() => true,
        [d, ..] if d.is_ascii_digit() => true,
        _ => false,
    };
    if needs_prefix {
        out.insert(0, '_');
    }
    out
}

/// Drop declarations whose value matches the CSS spec default (a no-op both
/// in the stylesheet and inline). Mirrors calibre's `simplify_styles` at the
/// high-impact level:
pub fn prune_default_decls(decl: &mut CssDecl) {
    decl.items.retain(|(k, v)| !is_default_decl(k, v));
}

fn is_default_decl(name: &str, value: &str) -> bool {
    let v = value.trim();
    match name {
        "letter-spacing" | "word-spacing" => {
            matches!(v, "0" | "0em" | "0px" | "0rem" | "normal")
        }
        "text-indent" => matches!(v, "0" | "0em" | "0px" | "0rem" | "0%"),
        "white-space" | "font-style" | "font-weight" | "font-variant" | "font-stretch" => {
            v == "normal"
        }
        "text-decoration" | "text-transform" => v == "none",
        _ => false,
    }
}

/// Promote inline-style declarations that repeat across elements into
/// auto-generated class rules.
pub fn promote_repeated_inline_styles(
    inline_decls: impl IntoIterator<Item = String>,
    generated: &mut Vec<(String, CssDecl)>,
) -> HashMap<String, String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for s in inline_decls {
        *counts.entry(s).or_insert(0) += 1;
    }
    // Most frequent first; ties broken by the serialized text so class
    // numbering is deterministic run-to-run.
    let mut sorted: Vec<(String, usize)> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut promoted: HashMap<String, String> = HashMap::new();
    for (style_str, count) in sorted {
        if count < 2 {
            break; // Remaining entries are all single-occurrence.
        }
        let class_name = format!("g{}", generated.len());
        // Rebuild a CssDecl from the serialized string so the rule is
        // emitted via the same path as named-style rules.
        let decl = parse_inline_decl(&style_str);
        generated.push((class_name.clone(), decl));
        promoted.insert(style_str, class_name);
    }
    promoted
}

/// Everything the stylesheet emitter needs, gathered by either route.
#[derive(Debug, Default)]
pub struct StylesheetDoc {
    /// Image-based fixed-layout book: emit the viewport-fit reset instead of
    /// the reflowable body defaults.
    pub fixed_layout: bool,
    /// Doc-level CSS writing mode (`horizontal-tb` emits no body rule).
    pub writing_mode: String,
    /// Named rules: (raw source style name, declarations). Emitted sorted by
    pub named_rules: Vec<(String, CssDecl)>,
    /// State-conditional rules: (raw source style name, pseudo-class,
    /// declarations). Emitted as `.name:pseudo { … }` after the base rule of
    /// the same name, which is the order the cascade reads them in.
    pub pseudo_rules: Vec<(String, String, CssDecl)>,
    /// Auto-generated `g<N>` classes from [`promote_repeated_inline_styles`],
    /// emitted after the named rules in insertion order.
    pub generated_classes: Vec<(String, CssDecl)>,
}

impl StylesheetDoc {
    /// Assemble the final `style.css` text.
    pub fn emit(&self) -> String {
        let mut s = String::new();
        s.push_str("@charset \"utf-8\";\n");

        if self.fixed_layout {
            s.push_str("html, body { margin: 0; padding: 0; writing-mode: horizontal-tb; }\n");
            s.push_str("body { text-align: center; }\n");
            s.push_str(
                "img { display: block; width: 100vw; height: 100vh; object-fit: contain; }\n",
            );
        } else if !self.writing_mode.is_empty() && self.writing_mode != "horizontal-tb" {
            s.push_str(&format!(
                "body {{ writing-mode: {wm}; -webkit-writing-mode: {wm}; -epub-writing-mode: {wm}; }}\n",
                wm = self.writing_mode
            ));
        }

        // One pass over every style name that has anything to say, base rule
        let mut names: Vec<&str> = self
            .named_rules
            .iter()
            .map(|(n, _)| n.as_str())
            .chain(self.pseudo_rules.iter().map(|(n, _, _)| n.as_str()))
            .collect();
        names.sort_unstable();
        names.dedup();
        for name in names {
            if let Some((_, decl)) = self.named_rules.iter().find(|(n, _)| n == name)
                && !decl.is_empty()
            {
                s.push_str(&format!(
                    ".{} {{ {} }}\n",
                    safe_class_name(name),
                    decl.to_inline()
                ));
            }
            let mut states: Vec<&(String, String, CssDecl)> = self
                .pseudo_rules
                .iter()
                .filter(|(n, _, _)| n == name)
                .collect();
            states.sort_by(|a, b| a.1.cmp(&b.1));
            for (_, pseudo, decl) in states {
                if !decl.is_empty() {
                    s.push_str(&format!(
                        ".{}:{} {{ {} }}\n",
                        safe_class_name(name),
                        pseudo,
                        decl.to_inline()
                    ));
                }
            }
        }
        for (class_name, decl) in &self.generated_classes {
            if decl.is_empty() {
                continue;
            }
            s.push_str(&format!(".{} {{ {} }}\n", class_name, decl.to_inline()));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_class_name_sanitizes_and_prefixes() {
        assert_eq!(safe_class_name("HrDijd"), "HrDijd");
        assert_eq!(safe_class_name("0HrDijd"), "_0HrDijd");
        assert_eq!(safe_class_name("-9x"), "_-9x");
        assert_eq!(safe_class_name("-"), "_-");
        assert_eq!(safe_class_name("a.b c"), "a_b_c");
        assert_eq!(safe_class_name(""), "_");
    }

    /// Each name's state rules follow its own base rule, and a name whose
    /// only content is a state rule still reaches the sheet — otherwise a
    /// style that exists purely to colour links would vanish.
    #[test]
    fn state_rules_follow_their_base_rule() {
        let decl = |pairs: &[(&str, &str)]| {
            let mut d = CssDecl::new();
            for (k, v) in pairs {
                d.set(*k, *v);
            }
            d
        };
        let doc = StylesheetDoc {
            fixed_layout: false,
            writing_mode: "horizontal-tb".into(),
            named_rules: vec![
                ("sB".into(), decl(&[("font-style", "italic")])),
                ("sA".into(), CssDecl::new()),
            ],
            pseudo_rules: vec![
                ("sB".into(), "visited".into(), decl(&[("color", "purple")])),
                ("sA".into(), "link".into(), decl(&[("color", "blue")])),
                ("sB".into(), "link".into(), decl(&[("color", "blue")])),
            ],
            generated_classes: Vec::new(),
        };
        let css = doc.emit();
        let rules: Vec<&str> = css.lines().filter(|l| l.starts_with('.')).collect();
        assert_eq!(
            rules,
            vec![
                ".sA:link { color: blue }",
                ".sB { font-style: italic }",
                ".sB:link { color: blue }",
                ".sB:visited { color: purple }",
            ]
        );
    }

    /// Build `<body>` → the given chain of `(tag, style)` boxes.
    fn plate_doc(chain: &[(&str, &str)]) -> (Dom, dom::NodeId, dom::NodeId) {
        let (mut dom, _html, head, body) = dom::new_book_part("t");
        let mut parent = body;
        for (tag, style) in chain {
            let el = dom.sub_element(parent, *tag);
            if !style.is_empty() {
                dom.get_mut(el).set("style", *style);
            }
            parent = el;
        }
        (dom, head, body)
    }

    /// A KFX full-page plate states `height: 100%` of a page template CSS has
    /// no equivalent of. Without a stated chain the percentage resolves
    /// against nothing and the plate falls back to its intrinsic size.
    #[test]
    fn a_page_plate_gets_a_chain_its_height_can_resolve_against() {
        let (mut dom, head, body) = plate_doc(&[("div", ""), ("div", ""), ("img", "height: 100%")]);
        anchor_page_plate_height(&mut dom, head, body);
        let out = dom.serialize(dom.root);
        assert!(out.contains("html, body { height: 100% }"), "{out}");
        assert_eq!(
            out.matches(r#"<div style="height: 100%">"#).count(),
            2,
            "{out}"
        );
        // Clamped width recomputes a replaced element's used height, so a
        // page narrower than the plate scales it whole instead of clipping.
        assert!(
            out.contains(r#"style="height: 100%; max-width: 100%""#),
            "{out}"
        );
    }

    /// The chain may only stand for the page when the plate is all the
    /// document holds; a sibling means those boxes are ordinary content and
    /// sizing them to the page would clip it.
    #[test]
    fn a_plate_with_a_sibling_is_left_alone() {
        let (mut dom, head, body) = plate_doc(&[("div", ""), ("img", "height: 100%")]);
        dom.sub_element(body, "p");
        anchor_page_plate_height(&mut dom, head, body);
        let out = dom.serialize(dom.root);
        assert!(!out.contains("html, body"), "{out}");
        assert!(!out.contains("max-width"), "{out}");
    }

    /// An image sized in the axis CSS can always resolve needs nothing added,
    /// and must not be handed a page-height chain that would resize it.
    #[test]
    fn a_width_sized_image_is_left_alone() {
        let (mut dom, head, body) = plate_doc(&[("div", ""), ("img", "width: 58.594%")]);
        anchor_page_plate_height(&mut dom, head, body);
        let out = dom.serialize(dom.root);
        assert!(!out.contains("html, body"), "{out}");
        assert!(out.contains(r#"style="width: 58.594%""#), "{out}");
    }

    #[test]
    fn prune_drops_spec_defaults_only() {
        let mut d = CssDecl::new();
        d.set("letter-spacing", "0em");
        d.set("text-indent", "0%");
        d.set("font-weight", "normal");
        d.set("text-decoration", "none");
        d.set("font-weight", "bold"); // last write wins → kept
        d.set("margin-top", "0"); // not in the prune table → kept
        prune_default_decls(&mut d);
        assert_eq!(d.to_inline(), "font-weight: bold; margin-top: 0");
    }

    #[test]
    fn promotion_threshold_and_ordering() {
        let inline = vec![
            "text-align: center".to_string(),
            "text-align: center".to_string(),
            "width: 100%".to_string(),
            "width: 100%".to_string(),
            "width: 100%".to_string(),
            "margin-top: 1em".to_string(), // single occurrence — stays inline
        ];
        let mut generated = Vec::new();
        let promoted = promote_repeated_inline_styles(inline, &mut generated);
        // Highest count first: width×3 → g0, center×2 → g1; the singleton
        // is not promoted.
        assert_eq!(promoted.get("width: 100%").map(String::as_str), Some("g0"));
        assert_eq!(
            promoted.get("text-align: center").map(String::as_str),
            Some("g1")
        );
        assert!(!promoted.contains_key("margin-top: 1em"));
        assert_eq!(generated.len(), 2);
        assert_eq!(generated[0].1.to_inline(), "width: 100%");
    }

    #[test]
    fn promotion_tie_breaks_by_text() {
        let inline = vec![
            "b: 2".to_string(),
            "b: 2".to_string(),
            "a: 1".to_string(),
            "a: 1".to_string(),
        ];
        let mut generated = Vec::new();
        let promoted = promote_repeated_inline_styles(inline, &mut generated);
        assert_eq!(promoted.get("a: 1").map(String::as_str), Some("g0"));
        assert_eq!(promoted.get("b: 2").map(String::as_str), Some("g1"));
    }

    #[test]
    fn emit_reflowable_vertical() {
        let mut doc = StylesheetDoc {
            writing_mode: "vertical-rl".to_string(),
            ..Default::default()
        };
        doc.named_rules.push(("zeta".into(), {
            let mut d = CssDecl::new();
            d.set("font-size", "1rem");
            d
        }));
        doc.named_rules.push(("alpha".into(), {
            let mut d = CssDecl::new();
            d.set("text-align", "justify");
            d
        }));
        doc.named_rules.push(("empty".into(), CssDecl::new()));
        doc.generated_classes.push(("g0".into(), {
            let mut d = CssDecl::new();
            d.set("width", "100%");
            d
        }));
        let css = doc.emit();
        assert_eq!(
            css,
            "@charset \"utf-8\";\n\
             body { writing-mode: vertical-rl; -webkit-writing-mode: vertical-rl; -epub-writing-mode: vertical-rl; }\n\
             .alpha { text-align: justify }\n\
             .zeta { font-size: 1rem }\n\
             .g0 { width: 100% }\n"
        );
    }

    #[test]
    fn emit_fixed_layout_reset() {
        let doc = StylesheetDoc {
            fixed_layout: true,
            writing_mode: "vertical-rl".to_string(),
            ..Default::default()
        };
        let css = doc.emit();
        assert!(css.starts_with("@charset \"utf-8\";\n"));
        assert!(css.contains(
            "img { display: block; width: 100vw; height: 100vh; object-fit: contain; }\n"
        ));
        // The FXL reset replaces (not joins) the body writing-mode rule.
        assert!(!css.contains("-epub-writing-mode"));
    }

    #[test]
    fn emit_horizontal_has_no_body_rule() {
        let doc = StylesheetDoc {
            writing_mode: "horizontal-tb".to_string(),
            ..Default::default()
        };
        assert_eq!(doc.emit(), "@charset \"utf-8\";\n");
    }
}
