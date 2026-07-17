//! IR chapter → shared XHTML DOM for the KFX normalized export.
//!
//! Builds each chapter through [`crate::export::xdom`] — the same DOM,
//! consolidation passes, and serializer the mechanical `kfx_to_epub` route
//! uses — so both KFX→EPUB engines ship byte-identical chapter files by
//! construction.
//!
//! The build mirrors the mechanical walk's attribute channels exactly:
//!
//! - **Walk-time attributes** (`href`, `src`, `alt`, `id`) go directly onto
//!   the element in the mechanical route's insertion order.
//! - **Block classes / inline styles** accumulate in pending maps and land
//!   *after* the walk via [`xdom::finalize_attrs`] — so a stamped `id`
//!   precedes `class` on block elements.
//! - **Inline-run classes** (styled spans, ruby, links — the mechanical
//!   route's `attach_inline_style` sites) are set at creation, so a
//!   later-stamped `id` follows `class` on those.
//!
//! KFX block containers all emit as `<div>`; the shared
//! [`xdom::consolidate_part`] performs the div→p leaf-text rename and the
//! `<h<N>>` promotion off the layout-hint map, exactly as the mechanical
//! route does. Roles that only exist for other sources (definition lists)
//! keep their natural tags — they never occur in KFX-sourced IR.

use std::collections::{HashMap, HashSet};

use crate::export::css::parse_inline_decl;
use crate::export::xdom::{self, Dom, LayoutHints};
use crate::model::{Chapter, NodeId as IrNodeId, Role};

use super::html_synth::{InlineStyleEmit, LinkOutcome, SourceStyles};

/// Per-chapter emission inputs for the KFX normalized path.
pub struct KfxChapterEmit<'a> {
    /// `<title>` text — the section name (mechanical: `push_book_part`'s
    /// `title` argument), NOT the deduped filename.
    pub title: &'a str,
    /// Book language for `<html xml:lang lang>` (skipped when empty).
    pub language: &'a str,
    /// Named-class / inline-style resolution built book-wide by
    /// `normalize_book` (same maps the string-synthesis path used).
    pub source_styles: &'a SourceStyles<'a>,
    /// Link resolution for `semantics.href` values.
    pub href_resolver: &'a dyn Fn(&str) -> LinkOutcome,
}

/// Build, consolidate, and serialize one KFX chapter document. Referenced
/// image paths are added to `assets`.
pub fn emit_kfx_chapter(
    ir: &Chapter,
    opts: &KfxChapterEmit<'_>,
    assets: &mut HashSet<String>,
) -> String {
    let (mut dom, html_id, head_id, body_id) = xdom::new_book_part(opts.title);
    // `xml:lang` + `lang` on `<html>` from the book language (mechanical:
    // `push_book_part`; calibre `set_doc_lang`).
    let lang = opts.language.trim();
    if !lang.is_empty() {
        dom.get_mut(html_id).set("xml:lang", lang);
        dom.get_mut(html_id).set("lang", lang);
    }
    // Stylesheet link — sibling filename, matching the mechanical route.
    let link = dom.sub_element(head_id, "link");
    let l = dom.get_mut(link);
    l.set("rel", "stylesheet");
    l.set("type", "text/css");
    l.set("href", "style.css");

    let mut b = Builder {
        ir,
        dom,
        classes: HashMap::new(),
        styles: HashMap::new(),
        hints: HashMap::new(),
        opts,
        assets,
    };
    for child in ir.children(crate::model::NodeId::ROOT) {
        b.walk(child, body_id);
    }

    let Builder {
        mut dom,
        classes,
        styles,
        hints,
        ..
    } = b;

    // Same pass order as the mechanical pipeline (`build_output`): links are
    // already resolved (walk-time), then consolidate → list normalization →
    // EOL → (styles were pruned/promoted book-wide upstream) → attribute
    // finalization → document assembly.
    xdom::consolidate_part(&mut dom, &classes, &styles, &hints);
    xdom::normalize_lists_dom(&mut dom);
    xdom::replace_eol_with_br_dom(&mut dom);
    xdom::finalize_attrs(&mut dom, &classes, &styles);
    xdom::chapter_document(&dom)
}

struct Builder<'a, 'b> {
    ir: &'a Chapter,
    dom: Dom,
    classes: HashMap<xdom::NodeId, Vec<String>>,
    styles: HashMap<xdom::NodeId, crate::export::css::CssDecl>,
    hints: LayoutHints,
    opts: &'a KfxChapterEmit<'b>,
    assets: &'a mut HashSet<String>,
}

impl Builder<'_, '_> {
    /// Append IR text lxml-style: onto the parent's leading text when it has
    /// no element children yet, else onto the last child's tail. This is the
    /// post-`strip_empty_spans` shape the mechanical route reaches — plain
    /// runs merge into the surrounding text slots.
    fn append_text(&mut self, parent: xdom::NodeId, text: &str) {
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
    fn attach_block_style(&mut self, id: IrNodeId, el: xdom::NodeId) {
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
    /// (mechanical `attach_inline_style`), so `strip_empty_spans` keeps the
    /// element and a later `id` lands after it.
    fn attach_inline_class(&mut self, id: IrNodeId, el: xdom::NodeId) {
        if let Some(class) = self.named_class(id) {
            self.dom.get_mut(el).set("class", class);
        }
    }

    fn stamp_id(&mut self, id: IrNodeId, el: xdom::NodeId) {
        if let Some(elem_id) = self.ir.semantics.id(id)
            && !self.dom.get(el).has_attr("id")
        {
            self.dom.get_mut(el).set("id", elem_id);
        }
    }

    fn walk(&mut self, id: IrNodeId, parent: xdom::NodeId) {
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
                self.stamp_id(id, ruby);
                // Base content wraps in `<rb>`; the annotation children
                // (`RubyText`) follow as `<rt>` — calibre's rb/rt shape.
                let children: Vec<IrNodeId> = self.ir.children(id).collect();
                let mut rb: Option<xdom::NodeId> = None;
                for child in children {
                    let is_rt = self
                        .ir
                        .node(child)
                        .is_some_and(|n| n.role == Role::RubyText);
                    if is_rt {
                        rb = None;
                        let rt = self.dom.sub_element(ruby, "rt");
                        // `<rt>` always exists, even with an empty
                        // annotation (mechanical: lookup miss → "").
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
                self.stamp_id(id, a);
                for child in self.ir.children(id).collect::<Vec<_>>() {
                    self.walk(child, a);
                }
            }
            Role::Inline => {
                let span = self.dom.sub_element(parent, "span");
                if self.ir.semantics.render_inline(id) {
                    // A demoted `render: inline` block keeps the block
                    // attribute channels (id at walk time, class/style via
                    // the pending maps) — the mechanical demotion retags the
                    // div but leaves its attribute plumbing alone.
                    self.stamp_id(id, span);
                    self.attach_block_style(id, span);
                } else {
                    self.attach_inline_class(id, span);
                    self.stamp_id(id, span);
                }
                for child in self.ir.children(id).collect::<Vec<_>>() {
                    self.walk(child, span);
                }
            }
            Role::Image => {
                let img = self.dom.sub_element(parent, "img");
                if let Some(src) = self.ir.semantics.src(id) {
                    self.dom.get_mut(img).set("src", src);
                    self.assets.insert(src.to_string());
                }
                // `alt` is always present (mechanical: calibre defaults "").
                let alt = self.ir.semantics.alt(id).unwrap_or("");
                self.dom.get_mut(img).set("alt", alt);
                self.stamp_id(id, img);
                self.attach_block_style(id, img);
            }
            Role::Break => {
                self.dom.sub_element(parent, "br");
            }
            _ => {
                let mut tag = self.block_tag(role);
                // Row children that would emit as `<div>` become `<td>` —
                // the mechanical route's calibre rule (`emit_table_row`).
                if tag == "div" && self.dom.get(parent).tag == "tr" {
                    tag = "td";
                }
                let el = self.dom.sub_element(parent, tag);
                self.stamp_id(id, el);
                self.attach_block_style(id, el);
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
    /// mechanical route's EPUB-2.0-gated promotions) all emit `<div>`;
    /// [`xdom::consolidate_part`] renames leaf-text divs to `<p>` and
    /// promotes hinted headings to `<h<N>>`.
    fn block_tag(&self, role: Role) -> &'static str {
        match role {
            Role::OrderedList => "ol",
            Role::UnorderedList => "ul",
            Role::ListItem => "li",
            Role::Table => "table",
            Role::TableHead => "thead",
            Role::TableBody => "tbody",
            Role::TableRow => "tr",
            // The mechanical route never emits `<th>` (calibre converts row
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
