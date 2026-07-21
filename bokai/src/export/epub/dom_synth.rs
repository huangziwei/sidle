//! DOM-synthesis regime of the normalized EPUB export: chapter documents
//! built through the shared XHTML DOM ([`super::dom`]), plus the stylesheet
//! machinery for source-declared style programs.
//!
//! `normalize_book` selects this regime when the importer supplies a
//! [`CssProgram`](crate::import::CssProgram) via
//! [`crate::import::Importer::stylesheet_program`]
//! (today only the KFX importer does); books without one go through the
//! string-synthesis regime in `super::synth`. Chapters run the same DOM,
//! consolidation passes, and serializer as the mechanical `kfx_to_epub`
//! route, so both KFX→EPUB engines ship byte-identical files by
//! construction.
//!
//! The chapter build mirrors the mechanical walk's attribute channels
//! exactly:
//!
//! - **Walk-time attributes** (`href`, `src`, `alt`, `id`) go directly onto
//!   the element in the mechanical route's insertion order.
//! - **Block classes / inline styles** accumulate in pending maps and land
//!   *after* the walk via [`dom::finalize_attrs`] — so a stamped `id`
//!   precedes `class` on block elements.
//! - **Inline-run classes** (styled spans, ruby, links — the mechanical
//!   route's `attach_inline_style` sites) are set at creation, so a
//!   later-stamped `id` follows `class` on those.
//!
//! Block containers all emit as `<div>`; the shared
//! [`dom::consolidate_part`] performs the div→p leaf-text rename and the
//! `<h<N>>` promotion off the layout-hint map, exactly as the mechanical
//! route does. Roles this regime never receives from its current source
//! (definition lists, code blocks) keep their natural tags.
//!
//! Stylesheet side: class-name sanitization ([`safe_class_name`]),
//! spec-default pruning ([`prune_default_decls`]), repeated-inline-style
//! promotion ([`promote_repeated_inline_styles`]), and final assembly
//! ([`StylesheetDoc::emit`]) — all over the raw declaration container
//! [`CssDecl`] from [`crate::style`].

use std::collections::{HashMap, HashSet};

use crate::model::{Chapter, NodeId as IrNodeId, Role};
use crate::style::parse_inline_decl;

use super::dom::{self, Dom, LayoutHints};
use super::normalize::{InlineStyleEmit, LinkOutcome, SourceElements, SourceStyles};

// Port-compat re-exports: the frozen mechanical port reaches these through
// its historical `export::css::` paths (this module's alias in
// `export/mod.rs`); their homes are `style::declaration` and
// `formats::kfx::yj_properties`. Deleted together with the port.
pub use crate::formats::kfx::yj_properties::partition_image_style;
pub use crate::style::CssDecl;

/// Per-chapter emission inputs for the DOM-synthesis path.
pub struct ChapterEmit<'a> {
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
    /// Fixed-layout page pixel viewport → `<meta name="viewport">` in the
    /// head. `None` for reflowable documents.
    pub viewport: Option<(u32, u32)>,
    /// Whether to stamp `data-eid` from `semantics.source_element`.
    pub source_elements: SourceElements,
    /// The assets the source actually holds, when the importer can enumerate
    /// them. An `<img>` whose `src` is not in this set degrades to a `<span>`
    /// carrying its alt text — see [`Builder::emit_image`].
    ///
    /// `None` disables the check, which is what a caller that cannot name the
    /// container's contents should pass; guessing would drop real images.
    pub available_assets: Option<&'a HashSet<String>>,
}

/// Build, consolidate, and serialize one chapter document. Referenced
/// image paths are added to `assets`.
pub fn emit_chapter(ir: &Chapter, opts: &ChapterEmit<'_>, assets: &mut HashSet<String>) -> String {
    let (mut dom, html_id, head_id, body_id) = dom::new_book_part(opts.title);
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
    // Fixed-layout page viewport, after the stylesheet link (mechanical:
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
    };
    for child in ir.children(crate::model::NodeId::ROOT) {
        b.walk(child, body_id);
    }

    let Builder {
        mut dom,
        mut classes,
        mut styles,
        mut hints,
        ..
    } = b;

    // A document whose whole content is one container: that container IS the
    // page, so it becomes `<body>` rather than a box inside it. See
    // [`merge_sole_container_into_body`].
    merge_sole_container_into_body(&mut dom, body_id, &mut classes, &mut styles, &mut hints);

    // Same pass order as the mechanical pipeline (`build_output`): links are
    // already resolved (walk-time), then consolidate → list normalization →
    // EOL → (styles were pruned/promoted book-wide upstream) → attribute
    // finalization → document assembly.
    dom::consolidate_part(&mut dom, &classes, &styles, &hints);
    dom::normalize_lists_dom(&mut dom);
    dom::replace_eol_with_br_dom(&mut dom);
    dom::finalize_attrs(&mut dom, &classes, &styles);
    dom::chapter_document(&dom)
}

/// Adopt a document's sole top-level container as its `<body>`.
///
/// A source that frames each document in one container (KFX gives every
/// section a page template) is describing the page, not a box on it. Emitting
/// it as a child of `<body>` puts its padding inside the page margin instead
/// of being it, and — where the container names a writing mode running across
/// the book's — makes an orthogonal flow, which shrink-wraps to its content
/// and sits at one page edge rather than filling the page.
///
/// The container's style channels move with it, so the emitted document is
/// the same one with a level removed.
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
}

impl Builder<'_, '_> {
    /// Append IR text lxml-style: onto the parent's leading text when it has
    /// no element children yet, else onto the last child's tail. This is the
    /// post-`strip_empty_spans` shape the mechanical route reaches — plain
    /// runs merge into the surrounding text slots.
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
    /// (mechanical `attach_inline_style`), so `strip_empty_spans` keeps the
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

    /// Emit an image, or the degradation for one whose bytes aren't there.
    ///
    /// A source can reference an image it does not ship: an `<img src>` whose
    /// extension disagrees with the manifest entry, or a spine that cites more
    /// images than the container holds. KFX does it too, where the cited
    /// `bcRawMedia` is absent.
    ///
    /// Carrying the reference through would put an `<img src>` in the output
    /// naming a file the container has no entry for — epubcheck RSC-007, and a
    /// broken-image box where content should be. Emitting a `<span>` with the
    /// alt text keeps whatever semantic content the source gave and references
    /// nothing. This matches the mechanical route's long-standing behavior for
    /// KFX; it applies here to every source because the rule is about what a
    /// container may contain, not about where the book came from.
    fn emit_image(&mut self, id: IrNodeId, parent: dom::NodeId) {
        let src = self.ir.semantics.src(id);
        // `alt` is always present (mechanical: calibre defaults "").
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
        self.attach_block_style(id, img);
    }

    /// Carry the node's source element id onto its DOM element, so a renderer
    /// can resolve an `(element, offset)` handle by querying `[data-eid]` and
    /// walking text from there. Precedes [`Self::stamp_id`] at every call site
    /// — the mechanical route stamps the eid before the position anchor, and
    /// matching attribute order keeps the two routes' documents comparable.
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
                self.stamp_source_element(id, a);
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
                    self.stamp_source_element(id, span);
                    self.stamp_id(id, span);
                    self.attach_block_style(id, span);
                } else {
                    self.attach_inline_class(id, span);
                    self.stamp_source_element(id, span);
                    self.stamp_id(id, span);
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
                // the mechanical route's calibre rule (`emit_table_row`).
                if tag == "div" && self.dom.get(parent).tag == "tr" {
                    tag = "td";
                }
                let el = self.dom.sub_element(parent, tag);
                self.stamp_source_element(id, el);
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
    /// [`dom::consolidate_part`] renames leaf-text divs to `<p>` and
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
/// Sanitize a source style name into a valid CSS class name (and matching HTML
/// `class` attribute). Non-identifier characters become `_`; a leading digit
/// (or `-digit` / lone `-`) is prefixed with `_`, since a CSS identifier can't
/// start with a digit — an unescaped `.0HrDijd…` selector is a parse error
/// (epubcheck CSS-008). Applied identically to the selector and the element's
/// class attribute so they stay in sync.
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
///   - `letter-spacing` / `word-spacing`: `0` / `0em` / `0px` / `0rem` /
///     `normal` → drop
///   - `text-indent`: `0` / `0em` / `0px` / `0rem` / `0%` → drop
///   - `white-space` / `font-style` / `font-weight` / `font-variant` /
///     `font-stretch`: `normal` → drop
///   - `text-decoration` / `text-transform`: `none` → drop
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
///
/// Mirrors a subset of calibre's `fixup_styles_and_classes`
/// (yj_to_epub_properties.py:1388): when the same serialized declaration
/// shows up on ≥ 2 elements across the book, it gets a `g<N>` class rule and
/// the caller replaces each matching inline style with the class reference.
/// Single-occurrence styles stay inline — keeps the stylesheet readable.
///
/// `inline_decls` is the multiset of serialized non-empty inline styles
/// (one entry per styled element). Promoted rules are appended to
/// `generated` (numbering continues from its current length so class names
/// stay stable); the returned map is serialized-style → class name for the
/// caller's rewrite pass.
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
    /// name, selectors sanitized via [`safe_class_name`]; empty declarations
    /// are skipped (a class attribute may still reference them — the rule is
    /// simply absent, which renders identically).
    pub named_rules: Vec<(String, CssDecl)>,
    /// Auto-generated `g<N>` classes from [`promote_repeated_inline_styles`],
    /// emitted after the named rules in insertion order.
    pub generated_classes: Vec<(String, CssDecl)>,
}

impl StylesheetDoc {
    /// Assemble the final `style.css` text.
    ///
    /// Layout matches calibre's output: `@charset` first; for fixed-layout
    /// books a reset that sizes images to the viewport (the page wrapper
    /// establishes no definite height, and a vertical-rl body would flip the
    /// block axis — page-turn direction is carried by
    /// `page-progression-direction`, so forcing horizontal-tb is safe); for
    /// reflowable books a `body { writing-mode: … }` rule when the book is
    /// not horizontal-tb. Then one rule per named style (sorted), then the
    /// generated classes.
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

        let mut named: Vec<&(String, CssDecl)> = self.named_rules.iter().collect();
        named.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, decl) in named {
            if decl.is_empty() {
                continue;
            }
            s.push_str(&format!(
                ".{} {{ {} }}\n",
                safe_class_name(name),
                decl.to_inline()
            ));
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
