//! Phase 1 step 4: storyline → XHTML.
//!
//! Mechanical port of calibre's `yj_to_epub_content.py`. The orchestrator
//! drives this via `process_reading_order`, which walks each
//! `reading_order.sections[]` → `section.page_templates[]` →
//! `page_template.content_list[]` → `content` (recursive) → XHTML body.
//!
//! `process_content` is the main recursive function; its `content_type`
//! match has many branches, one per KFX content type, each emitting the
//! appropriate XHTML element with the right attributes/style.
//!
//! Ruby (one of phase 1's hardest gates) is handled via `style_events`
//! attached to text containers: each event names a ruby_content fragment
//! whose annotation text becomes the `<rt>`, and the base text under the
//! event becomes `<rb>`.

#![allow(non_snake_case)]

use std::collections::HashMap;

use crate::kfx::container::get_field;
use crate::kfx::ion::IonValue;
use crate::kfx::symbols::KfxSymbol;

use super::dom::{Dom, NodeId};
use super::loader::BookData;
use super::properties::{self, CssDecl};
use super::resources::ResourceIndex;
use super::ConvertError;

/// Per-book state for content emission. Mirrors the instance attributes
/// calibre's `KFX_EPUB_Content.__init__` sets up.
pub struct ContentState<'a> {
    pub book: &'a BookData,
    pub resources: &'a ResourceIndex,

    /// Doc-level writing mode (from process_document_data).
    pub writing_mode: String,
    /// Page progression direction.
    pub page_progression_direction: String,

    /// Stylesheet (style_name → CssDecl) accumulated across all chapters.
    pub stylesheet: HashMap<String, CssDecl>,

    /// Auto-generated classes created by `fixup_styles_and_classes` when the
    /// same inline-style declaration appears on N elements. Each entry is
    /// `(class_name, decl)`; emitted as `.<class_name> { ... }` by
    /// `emit_stylesheet`. Kept separate from `stylesheet` because the keys
    /// in the latter are KFX style names (emitted as `.s_<name>`), whereas
    /// generated classes use a `g<N>` prefix.
    pub generated_classes: Vec<(String, CssDecl)>,

    /// Output book parts: filename → DOM. Insertion-ordered.
    pub book_parts: Vec<BookPart>,

    /// Mark anchors by location id → list of (offset, target NodeId in body).
    /// Used after content emission to wire up internal href targets.
    pub anchor_targets: HashMap<(String, i64), AnchorTarget>,

    /// Per-element-id → chapter filename. Populated during content emission as
    /// we encounter elements with an `id` ($155) anywhere in the storyline
    /// (including page_templates, container/text/etc.). Drives nav resolution
    /// in `navigation::extract_toc`: a nav_unit `target_position.id` maps to
    /// the chapter file containing that element.
    pub element_id_to_filename: HashMap<i64, String>,

    /// `$266 anchor` table — `(location_id, offset) → anchor_name(s)`.
    /// `process_content` consults this after dispatching a content struct
    /// and sets `id="anchor-name"` on the corresponding HTML element so
    /// (a) NCX entries pointing at `(eid, offset)` can use a fragment
    /// id, and (b) internal `<a href>` `link_to` references can resolve
    /// to the right element instead of the chapter file.
    pub anchors: super::navigation::AnchorTable,

    /// Mapping for the "main_content" link id and similar markers.
    pub link_ids: HashMap<String, String>,

    /// Tracks which KFX styles have been used (for emit-only-used-classes).
    pub used_kfx_styles: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AnchorTarget {
    pub book_part_index: usize,
    pub element_id: String,
}

pub struct BookPart {
    pub filename: String,
    pub dom: Dom,
    pub html_id: NodeId,
    pub head_id: NodeId,
    pub body_id: NodeId,
    /// Per-part class assignments (so the stylesheet pass can collect them).
    pub element_classes: HashMap<NodeId, Vec<String>>,
    /// Inline styles on each node (resolved before serialise).
    pub element_styles: HashMap<NodeId, CssDecl>,
    /// element id attribute assignments (anchor_name → NodeId).
    pub element_ids: HashMap<String, NodeId>,
    /// KFX `$761 layout_hints` + `$790 heading_level` carried from the
    /// element's named style. Read in `consolidate_html` to promote
    /// `<div>` → `<h<N>>` / `<figure>` (calibre `yj_to_epub_properties.py:1921`).
    pub element_layout_hints: HashMap<NodeId, (Vec<String>, Option<String>)>,
}

impl<'a> ContentState<'a> {
    pub fn new(book: &'a BookData, resources: &'a ResourceIndex) -> Self {
        let (writing_mode, page_progression_direction) = extract_doc_data(book);
        let anchors = super::navigation::extract_anchors(book);
        Self {
            book,
            resources,
            writing_mode,
            page_progression_direction,
            stylesheet: HashMap::new(),
            generated_classes: Vec::new(),
            book_parts: Vec::new(),
            anchor_targets: HashMap::new(),
            anchors,
            link_ids: HashMap::new(),
            used_kfx_styles: Vec::new(),
            element_id_to_filename: HashMap::new(),
        }
    }

    /// Entry point: walk every reading_order → section → page_template and
    /// emit one XHTML book_part per section. Mirrors
    /// `KFX_EPUB_Content.process_reading_order`.
    pub fn process_reading_order(&mut self) -> Result<(), ConvertError> {
        let reading_orders = extract_reading_orders(self.book);
        let mut used_sections: Vec<String> = Vec::new();

        for order in reading_orders {
            for section_name in order {
                if used_sections.contains(&section_name) {
                    continue;
                }
                self.process_section(&section_name)?;
                used_sections.push(section_name);
            }
        }
        Ok(())
    }

    fn process_section(&mut self, section_name: &str) -> Result<(), ConvertError> {
        let Some(section) = lookup_fragment(self.book, KfxSymbol::Section, section_name) else {
            eprintln!("kfx_to_epub: missing section {section_name}");
            return Ok(());
        };
        let inner = section.unwrap_annotated();
        let Some(fields) = inner.as_struct() else {
            return Ok(());
        };
        let page_templates: Vec<IonValue> = get_field(fields, KfxSymbol::PageTemplates as u64)
            .and_then(|v| v.as_list())
            .map(|v| v.to_vec())
            .unwrap_or_default();
        if page_templates.is_empty() {
            return Ok(());
        }

        // Calibre's "main page_template": the last one in the list.
        let filename = format!("{section_name}.xhtml");
        let part_index = self.book_parts.len();
        let (mut dom, html_id, head_id, body_id) = super::dom::new_book_part(section_name);
        // `xml:lang` on `<html>` — calibre adds it on every spine doc using
        // the book-level `dc:language` (epub_output.py: `set_doc_lang`).
        // Reading systems use this for font selection and word-break.
        let lang = self.book.metadata.language.trim();
        if !lang.is_empty() {
            dom.get_mut(html_id).set("xml:lang", lang);
            dom.get_mut(html_id).set("lang", lang);
        }
        self.book_parts.push(BookPart {
            filename: filename.clone(),
            dom,
            html_id,
            head_id,
            body_id,
            element_classes: HashMap::new(),
            element_styles: HashMap::new(),
            element_ids: HashMap::new(),
            element_layout_hints: HashMap::new(),
        });

        // Link stylesheet.
        self.link_stylesheet(part_index);

        // Record every element id reachable from this section (page_templates +
        // their storylines, recursively). This lets `navigation::extract_toc`
        // resolve `nav_unit.target_position.id` to the chapter file the
        // navigation entry belongs in.
        let mut ids: Vec<i64> = Vec::new();
        for tpl in &page_templates {
            collect_element_ids(tpl, self.book, &mut ids);
        }
        for eid in ids {
            self.element_id_to_filename
                .entry(eid)
                .or_insert_with(|| filename.clone());
        }

        // Process the LAST page_template into the existing body element
        // (calibre's main path; conditional templates are prepended).
        let writing_mode = self.writing_mode.clone();
        let main_template = page_templates.last().cloned().unwrap();
        self.process_content(
            &main_template,
            part_index,
            body_id,
            &writing_mode,
            None,
            true,
        )?;
        Ok(())
    }

    fn link_stylesheet(&mut self, part_index: usize) {
        let part = &mut self.book_parts[part_index];
        let link = part.dom.sub_element(part.head_id, "link");
        let l = part.dom.get_mut(link);
        l.set("rel", "stylesheet");
        l.set("type", "text/css");
        // Plain filename: the chapter lives in `OEBPS/`, the stylesheet is
        // also bundled in `OEBPS/`, so a sibling reference is the correct
        // resolution. The earlier `../OEBPS/style.css` resolved to the same
        // file mathematically but tripped Apple Books (which silently
        // declined to load it), leaving the body without
        // `writing-mode: vertical-rl` — pages then rendered horizontal.
        l.set("href", "style.css");
    }

    /// Recursive content walker. `parent_id` is the DOM node we append into.
    /// `is_top_level` mirrors calibre's check that the parent is the HTML
    /// root (so we know to set tag to "body" rather than "div" at the top).
    fn process_content(
        &mut self,
        content: &IonValue,
        part_index: usize,
        parent_id: NodeId,
        writing_mode: &str,
        content_layout: Option<&str>,
        is_top_level: bool,
    ) -> Result<(), ConvertError> {
        let inner = content.unwrap_annotated();

        // IonString: emit a span containing the literal text.
        if let IonValue::String(s) = inner {
            let span = self.book_parts[part_index].dom.sub_element(parent_id, "span");
            self.book_parts[part_index].dom.get_mut(span).text = Some(s.clone());
            return Ok(());
        }
        // IonSymbol: resolve to a content fragment and recurse.
        if let IonValue::Symbol(id) = inner {
            let name = self.book.symbols.resolve(*id).to_string();
            if let Some(fragment) =
                lookup_fragment(self.book, KfxSymbol::Structure, &name)
            {
                return self.process_content(
                    &fragment,
                    part_index,
                    parent_id,
                    writing_mode,
                    content_layout,
                    is_top_level,
                );
            }
            // Fall through; unknown symbol.
            return Ok(());
        }

        let Some(fields) = inner.as_struct() else {
            return Ok(());
        };

        let content_type_sym = get_field(fields, KfxSymbol::Type as u64);
        let content_type = content_type_sym
            .and_then(|v| self.book.symbols.text_of(v))
            .unwrap_or("")
            .to_string();

        // Pull writing-mode override.
        let wm = get_field(fields, KfxSymbol::WritingMode as u64)
            .and_then(|v| self.book.symbols.text_of(v))
            .map(|s| match s {
                "horizontal_tb" => "horizontal-tb".to_string(),
                "vertical_rl" => "vertical-rl".to_string(),
                "vertical_lr" => "vertical-lr".to_string(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| writing_mode.to_string());

        // Apply kfx_style. The field key on content structs is `$style`
        // ($157, KfxSymbol::Style — same symbol id used as the by_type key
        // for `$style` entities). The previously-used `$style_name` ($173,
        // KfxSymbol::StyleName) is a different field used in a different
        // context and never matched any content element, which is why no
        // classes were emitted despite 78 `$style` entities being defined.
        let style_name = get_field(fields, KfxSymbol::Style as u64)
            .and_then(|v| self.book.symbols.text_of(v))
            .map(|s| s.to_string());

        let elem_id = match content_type.as_str() {
            // $269 text: a div container.
            "text" => self.emit_text_container(fields, part_index, &wm, &style_name)?,
            // $271 image.
            "image" => self.emit_image(fields, part_index)?,
            // $270 container (the most common wrapper).
            "container" => self.emit_container(fields, part_index, &wm, &style_name)?,
            // $276 list / $277 list_item.
            "list" => self.emit_list(fields, part_index, &wm, &style_name)?,
            "list_item" => self.emit_list_item(fields, part_index, &wm, &style_name)?,
            // $278 table.
            "table" => self.emit_table(fields, part_index, &wm, &style_name)?,
            // $454 tbody / $151 thead / $455 tfoot / $279 tr.
            "table_body" => self.emit_simple_container(fields, part_index, &wm, &style_name, "tbody")?,
            "table_head" => self.emit_simple_container(fields, part_index, &wm, &style_name, "thead")?,
            "table_foot" => self.emit_simple_container(fields, part_index, &wm, &style_name, "tfoot")?,
            "table_row" => self.emit_table_row(fields, part_index, &wm, &style_name)?,
            // $596 hr.
            "horizontal_rule" => self.emit_void(fields, part_index, &style_name, "hr")?,
            // $272 SVG container — emit minimal svg.
            "kvg_container" => self.emit_svg_container(fields, part_index, &wm)?,
            // $439 excerpt: hidden div.
            "excerpt" => self.emit_excerpt(fields, part_index, &wm, &style_name)?,
            // Unknown / not yet ported: emit a div with the children, log.
            other => {
                eprintln!("kfx_to_epub: content type {other:?} not yet ported, emitting div");
                self.emit_simple_container(fields, part_index, &wm, &style_name, "div")?
            }
        };

        // `$615 yj.classification` — calibre `yj_to_epub_content.py:1058`.
        // EPUB-2.0 active branches (we emit EPUB 2.0):
        //   - `$453 caption` → rename to `<caption>` when the parent is
        //     `<table>` (the only place an EPUB2 reader will style it).
        // EPUB-3-only branches (skipped here; would be `<aside>` /
        // `role="math"`):
        //   - `$281 footnote`, `$618 yj.chapternote` → `<aside epub:type="footnote">`
        //   - `$619 yj.endnote` → `<aside epub:type="endnote">`
        //   - `$688 math` → `role="math"`
        //   - `$689` is a documented no-op in calibre.
        if let Some(class_val) = get_field(fields, KfxSymbol::YjClassification as u64)
            && let Some(class_name) = self.book.symbols.text_of(class_val)
        {
            let dom_ref = &mut self.book_parts[part_index].dom;
            let parent_tag = dom_ref.get(parent_id).tag.clone();
            if class_name == "caption"
                && dom_ref.get(elem_id).tag == "div"
                && parent_tag == "table"
            {
                dom_ref.get_mut(elem_id).tag = "caption".to_string();
            }
            // Other classifications: tracked for parity with calibre but
            // intentionally non-promoting under EPUB 2.0.
            let _ = class_name;
        }

        // `process_position` — calibre `yj_to_epub_navigation.py:375`.
        // Every content struct with `$155 id` registers itself at
        // `(id, offset=0)`; if the anchor table has a position match
        // there, the element gets `id="anchor-id"`. This is what wires
        // NCX `<content src="chapter.xhtml#X">` to the right paragraph
        // and what lets internal `<a href>` resolve to fragments.
        // Partial-offset positions (offset > 0 from `locate_offset`)
        // are deferred to task #11 (split_span for partial-text ruby
        // covers the same machinery).
        if let Some(loc_id) = get_field(fields, KfxSymbol::Id as u64).and_then(|v| v.as_int())
            && let Some(anchor_id) = self.anchors.id_at(loc_id, 0)
        {
            let dom_ref = &mut self.book_parts[part_index].dom;
            // Don't overwrite an id that was already set (e.g. an
            // earlier walk through the same node).
            if !dom_ref.get(elem_id).attrs.iter().any(|(k, _)| k == "id") {
                dom_ref.get_mut(elem_id).set("id", anchor_id);
            }
        }

        // `$179 link_to`: wrap the emitted element in an `<a>`. Calibre
        // (`yj_to_epub_content.py:1268`) emits a placeholder anchor URI
        // (`anchor:NAME`) at this point; the post-pass
        // `resolve_link_placeholders` rewrites it to
        // `chapter.xhtml#anchor-id` after all content is laid out.
        let wrapped_id = if let Some(link_sym) = get_field(fields, KfxSymbol::LinkTo as u64)
            && let Some(name) = self.book.symbols.text_of(link_sym)
        {
            let dom_ref = &mut self.book_parts[part_index].dom;
            let a = dom_ref.create_element("a");
            dom_ref.get_mut(a).set("href", format!("anchor:{}", name));
            dom_ref.append(a, elem_id);
            a
        } else {
            elem_id
        };

        // Always append into parent. For the top-level call, parent is the
        // existing body element; we don't retag (the body already exists).
        let dom = &mut self.book_parts[part_index].dom;
        if dom.get(wrapped_id).parent.is_none() {
            dom.append(parent_id, wrapped_id);
        }
        let _ = is_top_level;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Content-type emitters
    // -----------------------------------------------------------------

    fn emit_text_container(
        &mut self,
        fields: &[(u64, IonValue)],
        part_index: usize,
        writing_mode: &str,
        style_name: &Option<String>,
    ) -> Result<NodeId, ConvertError> {
        let dom = &mut self.book_parts[part_index].dom;
        let id = dom.create_element("div");
        // Apply class for style_name.
        self.attach_style(part_index, id, style_name, fields);

        self.add_content_children(fields, part_index, id, writing_mode)?;
        self.apply_style_events(fields, part_index, id, writing_mode)?;
        Ok(id)
    }

    fn emit_simple_container(
        &mut self,
        fields: &[(u64, IonValue)],
        part_index: usize,
        writing_mode: &str,
        style_name: &Option<String>,
        tag: &str,
    ) -> Result<NodeId, ConvertError> {
        let dom = &mut self.book_parts[part_index].dom;
        let id = dom.create_element(tag);
        self.attach_style(part_index, id, style_name, fields);
        self.add_content_children(fields, part_index, id, writing_mode)?;
        Ok(id)
    }

    fn emit_container(
        &mut self,
        fields: &[(u64, IonValue)],
        part_index: usize,
        writing_mode: &str,
        style_name: &Option<String>,
    ) -> Result<NodeId, ConvertError> {
        // $270 container: equivalent to a div for our purposes.
        self.emit_simple_container(fields, part_index, writing_mode, style_name, "div")
    }

    fn emit_excerpt(
        &mut self,
        fields: &[(u64, IonValue)],
        part_index: usize,
        writing_mode: &str,
        style_name: &Option<String>,
    ) -> Result<NodeId, ConvertError> {
        let id = self.emit_simple_container(fields, part_index, writing_mode, style_name, "div")?;
        self.book_parts[part_index]
            .element_styles
            .entry(id)
            .or_insert_with(CssDecl::new)
            .set("display", "none");
        Ok(id)
    }

    fn emit_image(
        &mut self,
        fields: &[(u64, IonValue)],
        part_index: usize,
    ) -> Result<NodeId, ConvertError> {
        let dom = &mut self.book_parts[part_index].dom;
        let id = dom.create_element("img");

        // Calibre: img_resource = self.process_external_resource(get_fragment_name(content, "$164"))
        // get_fragment_name(content, "$164") pops content["$175"] = resource_name.
        let resource_name = get_field(fields, KfxSymbol::ResourceName as u64)
            .and_then(|v| self.book.symbols.text_of(v))
            .unwrap_or("")
            .to_string();
        if let Some(img) = self.resources.by_name.get(&resource_name) {
            // Sibling reference: chapter and image both live under `OEBPS/`,
            // so the chapter resolves `image_rsrcXX.jpg` to the right path.
            // Earlier `../OEBPS/<file>` was mathematically equivalent but
            // tripped Apple Books, which then showed no images.
            dom.get_mut(id).set("src", img.filename.clone());
            if let Some(w) = img.width {
                let _ = w;
            }
        } else {
            dom.get_mut(id).set("src", format!("missing_{resource_name}.jpg"));
        }
        // Alt text. Calibre defaults to "" when missing.
        let alt = get_field(fields, KfxSymbol::AltText as u64)
            .and_then(|v| v.as_string())
            .unwrap_or("")
            .to_string();
        dom.get_mut(id).set("alt", alt);
        Ok(id)
    }

    fn emit_list(
        &mut self,
        fields: &[(u64, IonValue)],
        part_index: usize,
        writing_mode: &str,
        style_name: &Option<String>,
    ) -> Result<NodeId, ConvertError> {
        // Calibre picks "ol" vs "ul" based on $100 list_style_type. Simplify:
        // assume ul for now (most common in narrative books).
        let tag = list_tag_for(fields, &self.book.symbols);
        self.emit_simple_container(fields, part_index, writing_mode, style_name, tag)
    }

    fn emit_list_item(
        &mut self,
        fields: &[(u64, IonValue)],
        part_index: usize,
        writing_mode: &str,
        style_name: &Option<String>,
    ) -> Result<NodeId, ConvertError> {
        self.emit_simple_container(fields, part_index, writing_mode, style_name, "li")
    }

    fn emit_table(
        &mut self,
        fields: &[(u64, IonValue)],
        part_index: usize,
        writing_mode: &str,
        style_name: &Option<String>,
    ) -> Result<NodeId, ConvertError> {
        self.emit_simple_container(fields, part_index, writing_mode, style_name, "table")
    }

    fn emit_table_row(
        &mut self,
        fields: &[(u64, IonValue)],
        part_index: usize,
        writing_mode: &str,
        style_name: &Option<String>,
    ) -> Result<NodeId, ConvertError> {
        let id = self.emit_simple_container(fields, part_index, writing_mode, style_name, "tr")?;
        // Calibre converts unwrapped div children of tr into td. Apply.
        let children: Vec<NodeId> = self.book_parts[part_index].dom.get(id).children.clone();
        for c in children {
            if self.book_parts[part_index].dom.get(c).tag == "div" {
                self.book_parts[part_index].dom.get_mut(c).tag = "td".to_string();
            }
        }
        Ok(id)
    }

    fn emit_void(
        &mut self,
        _fields: &[(u64, IonValue)],
        part_index: usize,
        _style_name: &Option<String>,
        tag: &str,
    ) -> Result<NodeId, ConvertError> {
        Ok(self.book_parts[part_index].dom.create_element(tag))
    }

    fn emit_svg_container(
        &mut self,
        _fields: &[(u64, IonValue)],
        part_index: usize,
        _writing_mode: &str,
    ) -> Result<NodeId, ConvertError> {
        let dom = &mut self.book_parts[part_index].dom;
        let id = dom.create_element("svg");
        dom.get_mut(id).set("xmlns", "http://www.w3.org/2000/svg");
        dom.get_mut(id).set("version", "1.1");
        Ok(id)
    }

    // -----------------------------------------------------------------
    // Common: walk content children ($145 inline text, $146 child list,
    // $176 story reference). Mirrors calibre's `add_content`.
    // -----------------------------------------------------------------

    fn add_content_children(
        &mut self,
        fields: &[(u64, IonValue)],
        part_index: usize,
        parent_id: NodeId,
        writing_mode: &str,
    ) -> Result<(), ConvertError> {
        // $145 = text content (string or content_ref)
        if let Some(text_val) = get_field(fields, KfxSymbol::Content as u64) {
            let text = resolve_content_text(text_val, self.book);
            if self.try_emit_ruby_text(fields, part_index, parent_id, &text)? {
                return Ok(());
            }
            let dom = &mut self.book_parts[part_index].dom;
            let span = dom.sub_element(parent_id, "span");
            dom.get_mut(span).text = Some(text);
            return Ok(());
        }
        // $146 = content_list
        if let Some(list) = get_field(fields, KfxSymbol::ContentList as u64).and_then(|v| v.as_list()) {
            for child in list {
                self.process_content(child, part_index, parent_id, writing_mode, None, false)?;
            }
            return Ok(());
        }
        // $176 = story_name → process_story
        if let Some(story_name_sym) = get_field(fields, KfxSymbol::StoryName as u64)
            && let Some(story_name) = self.book.symbols.text_of(story_name_sym)
        {
            return self.process_story(story_name, part_index, parent_id, writing_mode);
        }
        Ok(())
    }

    /// Inline-event emission. Walks `$142 style_events` and emits each
    /// event as either a `<ruby>` (when `$757 ruby_name` is present) or
    /// an `<a href="anchor:NAME">` (when `$179 link_to` is present).
    /// Slices `text` at event boundaries; un-annotated runs become plain
    /// `<span>` children. Returns `true` if at least one event was
    /// emitted, so the caller can skip the plain-text fallback.
    fn try_emit_ruby_text(
        &mut self,
        fields: &[(u64, IonValue)],
        part_index: usize,
        parent_id: NodeId,
        text: &str,
    ) -> Result<bool, ConvertError> {
        let Some(events) =
            get_field(fields, KfxSymbol::StyleEvents as u64).and_then(|v| v.as_list())
        else {
            return Ok(false);
        };
        // RubyId can be Int (most common) or Symbol; we normalise both
        // to a string for lookup. Defined once here, used in both event
        // collection paths.
        let id_to_string = |v: &IonValue, syms: &super::loader::SymbolTable| -> Option<String> {
            match v.unwrap_annotated() {
                IonValue::Int(n) => Some(n.to_string()),
                IonValue::String(s) => Some(s.clone()),
                IonValue::Symbol(id) => Some(syms.resolve(*id).to_string()),
                _ => None,
            }
        };

        enum Ev {
            // (offset, length, ruby_name, id_list[(sub_off, sub_len, ruby_id)])
            Ruby(i64, i64, String, Vec<(i64, i64, String)>),
            // (offset, length, anchor_name) — the link target name; resolved
            // to a `chapter.xhtml#id` URI by `resolve_link_placeholders`.
            Link(i64, i64, String),
        }

        let mut collected: Vec<Ev> = Vec::new();
        for event in events {
            let Some(ef) = event.unwrap_annotated().as_struct() else {
                continue;
            };
            let Some(offset) = get_field(ef, KfxSymbol::Offset as u64).and_then(|v| v.as_int()) else {
                continue;
            };
            let Some(length) = get_field(ef, KfxSymbol::Length as u64).and_then(|v| v.as_int()) else {
                continue;
            };

            // Ruby event takes precedence (a single style_event in horror
            // never carries both ruby_name and link_to, but we'd render the
            // ruby annotation rather than the link if it did).
            if let Some(ruby_name) = get_field(ef, KfxSymbol::RubyName as u64)
                .and_then(|v| self.book.symbols.text_of(v))
            {
                let ruby_name = ruby_name.to_string();
                let id_list: Vec<(i64, i64, String)> = if let Some(id_val) =
                    get_field(ef, KfxSymbol::RubyId as u64)
                    && let Some(id_str) = id_to_string(id_val, &self.book.symbols)
                {
                    vec![(0, length, id_str)]
                } else if let Some(list) =
                    get_field(ef, KfxSymbol::RubyIdList as u64).and_then(|v| v.as_list())
                {
                    list.iter()
                        .filter_map(|entry| {
                            let f = entry.unwrap_annotated().as_struct()?;
                            let o = get_field(f, KfxSymbol::Offset as u64)?.as_int()?;
                            let l = get_field(f, KfxSymbol::Length as u64)?.as_int()?;
                            let id_str = id_to_string(
                                get_field(f, KfxSymbol::RubyId as u64)?,
                                &self.book.symbols,
                            )?;
                            Some((o, l, id_str))
                        })
                        .collect()
                } else {
                    continue;
                };
                collected.push(Ev::Ruby(offset, length, ruby_name, id_list));
            } else if let Some(link_sym) = get_field(ef, KfxSymbol::LinkTo as u64)
                && let Some(name) = self.book.symbols.text_of(link_sym)
            {
                collected.push(Ev::Link(offset, length, name.to_string()));
            }
        }

        if collected.is_empty() {
            return Ok(false);
        }
        collected.sort_by_key(|e| match e {
            Ev::Ruby(off, ..) | Ev::Link(off, ..) => *off,
        });

        let chars: Vec<char> = text.chars().collect();
        // Two-phase emit: separate link wrappers (which cover wide ranges,
        // typically a whole heading) from ruby wrappers (single-char,
        // nested inside the link's range). Without this split, a link
        // event at (0, N) advances the cursor past the whole text and
        // any ruby events inside `[0, N)` are silently skipped — the bug
        // that lost 5 ruby pairs after task #10 wired up link_to.
        let mut links: Vec<(usize, usize, String)> = Vec::new();
        let mut rubies: Vec<(usize, usize, String, Vec<(i64, i64, String)>)> = Vec::new();
        for ev in collected {
            match ev {
                Ev::Link(off, len, name) => {
                    let off = off as usize;
                    let len = len as usize;
                    if off + len > chars.len() {
                        continue;
                    }
                    links.push((off, len, name));
                }
                Ev::Ruby(off, len, name, id_list) => {
                    let off = off as usize;
                    let len = len as usize;
                    if off + len > chars.len() {
                        continue;
                    }
                    rubies.push((off, len, name, id_list));
                }
            }
        }

        // Helper: emit ruby/text pieces into `parent` for range [from, to).
        // Picks up any ruby event whose `[off, off+len)` falls inside the
        // range; leaves the rest as a `<span>`.
        let emit_range = |this: &mut Self, parent: NodeId, from: usize, to: usize| {
            let mut cursor = from;
            for (off, len, ruby_name, id_list) in &rubies {
                let off = *off;
                let len = *len;
                if off + len <= from || off >= to {
                    continue;
                }
                if off < cursor {
                    continue;
                }
                if off > cursor {
                    this.emit_span(part_index, parent, &chars[cursor..off]);
                }
                let ruby_el = this.book_parts[part_index].dom.sub_element(parent, "ruby");
                for (sub_off, sub_len, ruby_id_str) in id_list {
                    let sub_off = *sub_off as usize;
                    let sub_len = *sub_len as usize;
                    let slice_start = off + sub_off;
                    let slice_end = slice_start + sub_len;
                    if slice_end > chars.len() {
                        break;
                    }
                    let rb_text: String = chars[slice_start..slice_end].iter().collect();
                    let rb = this.book_parts[part_index].dom.sub_element(ruby_el, "rb");
                    this.book_parts[part_index].dom.get_mut(rb).text = Some(rb_text);

                    let rt_text = this.lookup_ruby_annotation(ruby_name, ruby_id_str);
                    let rt = this.book_parts[part_index].dom.sub_element(ruby_el, "rt");
                    this.book_parts[part_index].dom.get_mut(rt).text = Some(rt_text);
                }
                cursor = off + len;
            }
            if cursor < to {
                this.emit_span(part_index, parent, &chars[cursor..to]);
            }
        };

        // Walk the text top-to-bottom. Inside a link's range emit into
        // the `<a>`; outside, into the parent. Links don't nest in horror.
        let mut cursor: usize = 0;
        for (link_off, link_len, anchor_name) in &links {
            if *link_off < cursor {
                continue;
            }
            if *link_off > cursor {
                emit_range(self, parent_id, cursor, *link_off);
            }
            let a = self.book_parts[part_index].dom.sub_element(parent_id, "a");
            self.book_parts[part_index]
                .dom
                .get_mut(a)
                .set("href", format!("anchor:{}", anchor_name));
            emit_range(self, a, *link_off, *link_off + *link_len);
            cursor = *link_off + *link_len;
        }
        if cursor < chars.len() {
            emit_range(self, parent_id, cursor, chars.len());
        }
        Ok(true)
    }

    fn emit_span(&mut self, part_index: usize, parent: NodeId, chars: &[char]) {
        if chars.is_empty() {
            return;
        }
        let dom = &mut self.book_parts[part_index].dom;
        let span = dom.sub_element(parent, "span");
        dom.get_mut(span).text = Some(chars.iter().collect());
    }

    /// Look up the rt annotation text for a given ruby_name + ruby_id.
    /// `book_data["$393"]` (= `ruby_content`) maps ruby_name → struct with
    /// per-id text content.
    fn lookup_ruby_annotation(&self, ruby_name: &str, ruby_id: &str) -> String {
        let Some(ruby_map) = self.book.by_type.get(&(KfxSymbol::RubyContent as u64)) else {
            return String::new();
        };
        let Some(ruby_struct) = ruby_map.get(ruby_name) else {
            return String::new();
        };
        let inner = ruby_struct.unwrap_annotated();
        let Some(fields) = inner.as_struct() else {
            return String::new();
        };
        let Some(list) =
            get_field(fields, KfxSymbol::ContentList as u64).and_then(|v| v.as_list())
        else {
            return String::new();
        };
        for entry in list {
            let Some(ef) = entry.unwrap_annotated().as_struct() else {
                continue;
            };
            // Match RubyId as Int / String / Symbol — same normalisation
            // as the caller's id_to_string helper.
            let entry_id = match get_field(ef, KfxSymbol::RubyId as u64) {
                Some(IonValue::Int(n)) => n.to_string(),
                Some(IonValue::String(s)) => s.clone(),
                Some(IonValue::Symbol(id)) => self.book.symbols.resolve(*id).to_string(),
                _ => continue,
            };
            if entry_id != ruby_id {
                continue;
            }
            if let Some(text_val) = get_field(ef, KfxSymbol::Content as u64) {
                return resolve_content_text(text_val, self.book);
            }
        }
        String::new()
    }

    fn process_story(
        &mut self,
        story_name: &str,
        part_index: usize,
        parent_id: NodeId,
        writing_mode: &str,
    ) -> Result<(), ConvertError> {
        let Some(story) = lookup_fragment(self.book, KfxSymbol::Storyline, story_name) else {
            return Ok(());
        };
        let inner = story.unwrap_annotated();
        let Some(fields) = inner.as_struct() else {
            return Ok(());
        };
        if let Some(list) = get_field(fields, KfxSymbol::ContentList as u64).and_then(|v| v.as_list()) {
            let list_clone = list.to_vec();
            for child in &list_clone {
                self.process_content(child, part_index, parent_id, writing_mode, None, false)?;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Style + ruby handling
    // -----------------------------------------------------------------

    fn attach_style(
        &mut self,
        part_index: usize,
        elem_id: NodeId,
        style_name: &Option<String>,
        fields: &[(u64, IonValue)],
    ) {
        // 1. Style-name class.
        if let Some(name) = style_name {
            let decl = properties::style_decl_for(name, self.book);
            if !decl.is_empty() {
                if !self.used_kfx_styles.iter().any(|s| s == name) {
                    self.used_kfx_styles.push(name.clone());
                }
                self.stylesheet.entry(name.clone()).or_insert(decl);
                self.book_parts[part_index]
                    .element_classes
                    .entry(elem_id)
                    .or_default()
                    .push(format!("s_{}", safe_class_name(name)));
            }
            // Layout hints + heading level — drive the `<div>` → `<h<N>>`
            // promotion in `consolidate_html`. Not emitted as CSS (calibre
            // uses these as sentinels too and `simplify_styles` strips them).
            let (hints, level) = properties::style_layout_hints_for(name, self.book);
            if !hints.is_empty() || level.is_some() {
                self.book_parts[part_index]
                    .element_layout_hints
                    .insert(elem_id, (hints, level));
            }
        }
        // 1b. Layout hints + heading level can also be carried inline on the
        // content element's outer fields rather than on a named style entity.
        // boko's `export::kfx` writes them this way (storyline.rs adds
        // `$761 layout_hints` and `$790 yj.semantics.heading_level` to
        // `outer_fields`), so on a boko→boko roundtrip the named-style path
        // above never fires. Merge with anything from the named style so
        // both sources contribute.
        let (inline_hints, inline_level) =
            properties::layout_hints_from_element_fields(fields, &self.book.symbols);
        if !inline_hints.is_empty() || inline_level.is_some() {
            let entry = self.book_parts[part_index]
                .element_layout_hints
                .entry(elem_id)
                .or_default();
            for h in inline_hints {
                if !entry.0.iter().any(|existing| existing == &h) {
                    entry.0.push(h);
                }
            }
            if entry.1.is_none() {
                entry.1 = inline_level;
            }
        }
        // 2. Inline content properties (writing-mode, etc.).
        let inline = properties::convert_yj_properties(fields, &self.book.symbols, self.book);
        if !inline.is_empty() {
            self.book_parts[part_index]
                .element_styles
                .insert(elem_id, inline);
        }
    }

    /// Walk `$142 style_events`: each event names a chunk of text + a style.
    /// When the style includes `$757 ruby_name`, emit `<ruby>` with `<rb>`/
    /// `<rt>` children pulled from the named ruby_content fragments.
    fn apply_style_events(
        &mut self,
        fields: &[(u64, IonValue)],
        part_index: usize,
        _elem_id: NodeId,
        _writing_mode: &str,
    ) -> Result<(), ConvertError> {
        let Some(events) =
            get_field(fields, KfxSymbol::StyleEvents as u64).and_then(|v| v.as_list())
        else {
            return Ok(());
        };
        // Ruby handling: pure-port of the calibre $757 path. Other style
        // events (dropcap, link_to, etc.) are not yet wired.
        for event in events {
            let Some(event_fields) = event.unwrap_annotated().as_struct() else {
                continue;
            };
            // Ruby content reference.
            let _ruby_name = match get_field(event_fields, KfxSymbol::RubyName as u64)
                .and_then(|v| self.book.symbols.text_of(v))
            {
                Some(n) => n.to_string(),
                None => continue,
            };
            // Build ruby element. For now, we skip the full substitution
            // pass (which requires locate_offset on the rendered text);
            // the placeholder shows the structure is recognised.
            // TODO(phase1-step4-ruby): full ruby substitution via
            // find_or_create_style_event_element + locate_offset.
            let _ = part_index;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn extract_doc_data(book: &BookData) -> (String, String) {
    let mut writing_mode = "horizontal-tb".to_string();
    let mut page_progression_direction = "ltr".to_string();

    let Some(doc) = book.by_type.get(&(KfxSymbol::DocumentData as u64)) else {
        return (writing_mode, page_progression_direction);
    };
    let Some((_, value)) = doc.iter().next() else {
        return (writing_mode, page_progression_direction);
    };
    let inner = value.unwrap_annotated();
    let Some(fields) = inner.as_struct() else {
        return (writing_mode, page_progression_direction);
    };
    if let Some(wm) = get_field(fields, KfxSymbol::WritingMode as u64)
        .and_then(|v| book.symbols.text_of(v))
    {
        writing_mode = match wm {
            "horizontal_tb" => "horizontal-tb",
            "vertical_rl" => "vertical-rl",
            "vertical_lr" => "vertical-lr",
            other => other,
        }
        .to_string();
    }
    if let Some(dir) = get_field(fields, KfxSymbol::Direction as u64)
        .and_then(|v| book.symbols.text_of(v))
    {
        page_progression_direction = match dir {
            "ltr" => "ltr".to_string(),
            "rtl" => "rtl".to_string(),
            other => other.to_string(),
        };
    }
    // Calibre's writing-mode → ppd override (yj_to_epub_metadata.py:131): any
    // vertical-RL writing mode forces the page to read right-to-left, even if
    // the KFX `direction` field literally says `ltr` (which is the common case
    // for CJK vertical books).
    if writing_mode.ends_with("-rl") {
        page_progression_direction = "rtl".to_string();
    }
    (writing_mode, page_progression_direction)
}

fn extract_reading_orders(book: &BookData) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    // Try $538 document_data.reading_orders first, then $258 metadata.reading_orders.
    for type_id in [KfxSymbol::DocumentData as u64, KfxSymbol::Metadata as u64] {
        let Some(map) = book.by_type.get(&type_id) else {
            continue;
        };
        for (_, value) in map {
            let inner = value.unwrap_annotated();
            let Some(fields) = inner.as_struct() else {
                continue;
            };
            let Some(orders) =
                get_field(fields, KfxSymbol::ReadingOrders as u64).and_then(|v| v.as_list())
            else {
                continue;
            };
            for order in orders {
                let Some(order_fields) = order.as_struct() else {
                    continue;
                };
                let Some(sections) =
                    get_field(order_fields, KfxSymbol::Sections as u64).and_then(|v| v.as_list())
                else {
                    continue;
                };
                let mut names = Vec::new();
                for section in sections {
                    if let Some(name) = book.symbols.text_of(section) {
                        names.push(name.to_string());
                    }
                }
                if !names.is_empty() {
                    out.push(names);
                }
            }
            if !out.is_empty() {
                return out;
            }
        }
    }
    out
}

/// Look up a fragment of the given KFX type by name.
fn lookup_fragment(book: &BookData, ftype: KfxSymbol, fid: &str) -> Option<IonValue> {
    book.by_type
        .get(&(ftype as u64))
        .and_then(|m| m.get(fid))
        .cloned()
}

/// Walk every Ion value reachable from a page_template (including referenced
/// storylines) and append every `$155 id` value into `out`.
///
/// "Referenced storyline" = when an Ion struct has a `story_name` ($176)
/// field, look it up in `book.by_type[storyline]` and recurse into its
/// `content_list`. The walk is depth-first and idempotent against cycles
/// (tracks visited story names).
fn collect_element_ids(template: &IonValue, book: &BookData, out: &mut Vec<i64>) {
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    walk_ids_recursive(template, book, &mut visited, out);
}

fn walk_ids_recursive(
    value: &IonValue,
    book: &BookData,
    visited: &mut std::collections::HashSet<String>,
    out: &mut Vec<i64>,
) {
    let inner = value.unwrap_annotated();
    match inner {
        IonValue::Struct(fields) => {
            // Capture this struct's id field, if any.
            if let Some(id_value) = get_field(fields, KfxSymbol::Id as u64) {
                if let Some(n) = id_value.as_int() {
                    out.push(n);
                }
            }
            // Follow story_name references — these point at storylines in
            // book.by_type[storyline], whose content_list holds the actual
            // body of the chapter.
            if let Some(story_value) = get_field(fields, KfxSymbol::StoryName as u64) {
                if let Some(name) = book.symbols.text_of(story_value) {
                    if visited.insert(name.to_string()) {
                        if let Some(storyline) =
                            lookup_fragment(book, KfxSymbol::Storyline, name)
                        {
                            walk_ids_recursive(&storyline, book, visited, out);
                        }
                    }
                }
            }
            // Recurse into every field value (covers content_list, nested
            // structs, etc.).
            for (_, v) in fields {
                walk_ids_recursive(v, book, visited, out);
            }
        }
        IonValue::List(items) => {
            for item in items {
                walk_ids_recursive(item, book, visited, out);
            }
        }
        _ => {}
    }
}

/// Resolve `$145` text: either a literal string or a struct
/// `{name, $403: index}` pointing at a `book_data["$145"][name].$146[i]`.
fn resolve_content_text(value: &IonValue, book: &BookData) -> String {
    let inner = value.unwrap_annotated();
    if let Some(s) = inner.as_string() {
        return s.to_string();
    }
    if let Some(fields) = inner.as_struct() {
        let name = get_field(fields, KfxSymbol::Name as u64)
            .and_then(|v| book.symbols.text_of(v))
            .map(|s| s.to_string())
            .unwrap_or_default();
        let index = get_field(fields, KfxSymbol::Index as u64)
            .and_then(|v| v.as_int())
            .unwrap_or(0) as usize;
        if !name.is_empty()
            && let Some(content) = book.by_type.get(&(KfxSymbol::Content as u64))
            && let Some(entry) = content.get(&name)
            && let Some(list) = entry
                .unwrap_annotated()
                .as_struct()
                .and_then(|fs| get_field(fs, KfxSymbol::ContentList as u64))
                .and_then(|v| v.as_list())
            && let Some(item) = list.get(index)
            && let Some(s) = item.as_string()
        {
            return s.to_string();
        }
    }
    String::new()
}

fn list_tag_for(fields: &[(u64, IonValue)], symbols: &super::loader::SymbolTable) -> &'static str {
    // Calibre's LIST_STYLE_TYPES: only "ol" and "ul" matter for our pass.
    let style = get_field(fields, KfxSymbol::ListStyle as u64)
        .and_then(|v| symbols.text_of(v))
        .unwrap_or("");
    match style {
        "lower_alpha" | "upper_alpha" | "decimal" | "lower_roman" | "upper_roman" => "ol",
        _ => "ul",
    }
}

fn safe_class_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// HTML block-level elements (calibre's set in
/// `yj_to_epub_properties.py:1965`). Used by `consolidate_html` to decide
/// whether a `<div>` qualifies as a leaf-text paragraph.
const BLOCK_TAGS: &[&str] = &[
    "aside", "body", "caption", "div", "figure", "footer", "header", "main",
    "nav", "section", "article",
    "h1", "h2", "h3", "h4", "h5", "h6",
    "li", "ol", "ul", "dl", "dt", "dd",
    "p", "blockquote", "pre", "hr",
    "table", "thead", "tbody", "tfoot", "tr", "td", "th",
    "figcaption",
];

fn is_block_tag(tag: &str) -> bool {
    BLOCK_TAGS.iter().any(|t| *t == tag)
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
fn strip_empty_spans(dom: &mut super::dom::Dom) {
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

/// Port of calibre's `consolidate_html` (epub_output.py:742) +
/// div→p promotion (yj_to_epub_properties.py:1921).
///
/// Three passes per chapter:
/// 1. Strip attribute-less `<span>` (and class-less ones) — merges their
///    text/children into the parent so the spine isn't 90% `<span>` noise.
/// 2. Compute (has_block_desc, has_text_desc) per node.
/// 3. Rename leaf-text `<div>`s to `<p>` (no block child + has text).
///
/// Does NOT yet do `kfx-layout-hints: heading → h<N>` or `figure → figure`
/// promotions — those need YJ_PROPERTY_INFO entries for $761 / $790, owned
/// by step 6.
pub fn consolidate_html(state: &mut ContentState) {
    for part in &mut state.book_parts {
        strip_empty_spans(&mut part.dom);

        // First pass: compute (has_block_desc, has_text_desc) per node.
        let n = part.dom.len();
        let mut has_block_desc = vec![false; n];
        let mut has_text_desc = vec![false; n];
        // Reverse-post-order (children before parents): do iteratively.
        let mut order: Vec<NodeId> = Vec::with_capacity(n);
        let mut stack: Vec<NodeId> = vec![part.dom.root];
        while let Some(id) = stack.pop() {
            order.push(id);
            for &child in &part.dom.get(id).children {
                stack.push(child);
            }
        }
        // Process in reverse so children fold into parents.
        for id in order.iter().rev() {
            let elem = part.dom.get(*id);
            let mut block = has_block_desc[*id];
            let mut text = has_text_desc[*id];
            // Element's own text counts as text.
            if let Some(t) = &elem.text
                && t.chars().any(|c| !c.is_whitespace())
            {
                text = true;
            }
            for &child in &elem.children {
                let child_tag = part.dom.get(child).tag.clone();
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
                if let Some(tail) = &part.dom.get(child).tail
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
            let elem = part.dom.get_mut(id);
            if elem.tag == "div" && !has_block_desc[id] && has_text_desc[id] {
                elem.tag = "p".to_string();
            }
        }

        // Third pass: promote `<div>` / `<p>` to `<h<N>>` for elements whose
        // KFX style carries `$761 layout_hints` containing `heading`. The
        // heading level comes from `$790 yj.semantics.heading_level` (default
        // 1 if missing — calibre carries the "last seen" value forward in
        // `simplify_styles`, but that's an EPUB-output ordering concern;
        // defaulting to 1 is a safe fallback for now).
        //
        // Figure promotion (`<figure>`) is calibre-gated on `not epub2_desired`
        // — we're emitting EPUB 2.0, so we skip it. The `<figure>` element
        // is HTML5; most EPUB2 readers render it as a generic block, but the
        // calibre rule is the conservative choice.
        let layout_hints: Vec<(NodeId, Vec<String>, Option<String>)> = part
            .element_layout_hints
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
            let elem = part.dom.get_mut(id);
            if elem.tag != "div" && elem.tag != "p" {
                continue;
            }
            let lvl = level.as_deref().unwrap_or("1");
            elem.tag = format!("h{}", lvl);
        }
    }
}

/// Replace EOL characters (`\n` / `\r` / ` ` / ` `) inside
/// element text or tail with explicit `<br/>` elements. Mirrors calibre's
/// `replace_eol_with_br` (yj_to_epub_content.py:1720). KFX text content
/// carries forced line breaks as raw EOL characters; without this pass
/// they get collapsed by HTML whitespace rules and the source `<br/>`s
/// disappear from the rendered output.
///
/// Restart the per-part scan after each split because (a) the newly
/// created `<br/>`'s tail may itself contain more EOLs, (b) the
/// in-place insertion shifts indices.
pub fn replace_eol_with_br(state: &mut ContentState) {
    const EOL_CHARS: &[char] = &['\n', '\r', '\u{2028}', '\u{2029}'];
    for part in &mut state.book_parts {
        loop {
            let n = part.dom.len();
            let mut changed = false;
            for id in 0..n {
                // Element text — split at the first EOL, insert `<br/>` as
                // the new first child, drop the EOL.
                if let Some(text) = part.dom.get(id).text.clone()
                    && let Some(idx) = text.find(EOL_CHARS)
                {
                    let (head, rest) = text.split_at(idx);
                    let head = head.to_string();
                    let tail = rest.chars().skip(1).collect::<String>();
                    let br = part.dom.create_element("br");
                    part.dom.get_mut(br).tail = if tail.is_empty() { None } else { Some(tail) };
                    part.dom.get_mut(id).text =
                        if head.is_empty() { None } else { Some(head) };
                    part.dom.insert(id, 0, br);
                    changed = true;
                    break;
                }
                // Element tail — split, insert `<br/>` as the next sibling.
                if let Some(tail_text) = part.dom.get(id).tail.clone()
                    && let Some(idx) = tail_text.find(EOL_CHARS)
                {
                    let parent = match part.dom.get(id).parent {
                        Some(p) => p,
                        None => continue,
                    };
                    let pos = match part.dom.child_index(parent, id) {
                        Some(p) => p,
                        None => continue,
                    };
                    let (head, rest) = tail_text.split_at(idx);
                    let head = head.to_string();
                    let tail = rest.chars().skip(1).collect::<String>();
                    let br = part.dom.create_element("br");
                    part.dom.get_mut(br).tail = if tail.is_empty() { None } else { Some(tail) };
                    part.dom.get_mut(id).tail =
                        if head.is_empty() { None } else { Some(head) };
                    part.dom.insert(parent, pos + 1, br);
                    changed = true;
                    break;
                }
            }
            if !changed {
                break;
            }
        }
    }
}

/// After all chapters are emitted, fold per-element classes + inline styles
/// back onto the DOM as actual `class=` / `style=` attributes.
/// Rewrite `<a href="anchor:NAME">` placeholders emitted by `$179 link_to`
/// to their final `chapter.xhtml#anchor-id` form.
///
/// This is the post-pass equivalent of calibre's
/// `fixup_anchors_and_hrefs` `<a href>` rewrite (`yj_to_epub_navigation.py:493`).
/// Runs AFTER `process_reading_order` so `element_id_to_filename` is
/// fully populated (we need it to look up which chapter file a target
/// element lives in). Dangling anchors keep their placeholder href —
/// calibre logs them; we leave them so the validator's link-defects
/// gate surfaces them.
pub fn resolve_link_placeholders(state: &mut ContentState) {
    let map = state.element_id_to_filename.clone();
    let anchors = state.anchors.clone();
    for part in &mut state.book_parts {
        let n = part.dom.len();
        for id in 0..n {
            let attrs = part.dom.get(id).attrs.clone();
            for (k, v) in attrs {
                if k != "href" {
                    continue;
                }
                if let Some(name) = v.strip_prefix("anchor:")
                    && let Some(uri) = anchors.resolve_uri(name, &map)
                {
                    part.dom.get_mut(id).set("href", uri);
                }
            }
        }
    }
}

/// Drop declarations that match their CSS spec default value.
///
/// Minimal port of calibre's `simplify_styles` — the full version does
/// inheritance-aware partition (drop declarations that would inherit
/// the same value from an ancestor), but the per-element walk requires
/// reproducing the CSS cascade in Rust. The default-pruning pass is
/// the high-impact 80% of the win: declarations like
/// `letter-spacing: 0em` / `white-space: normal` / `text-indent: 0`
/// are no-ops that calibre's pass strips. On horror those three
/// account for ~150 redundant decls (~3 per stylesheet rule × 50
/// rules), shrinking the stylesheet without changing rendering.
///
/// Properties pruned to their spec default:
///   - `letter-spacing`: `0` / `0em` / `0px` → drop (default `normal`)
///   - `word-spacing`: `0` / `0em` / `0px` → drop (default `normal`)
///   - `text-indent`: `0` / `0em` / `0px` / `0%` → drop (default `0`)
///   - `white-space`: `normal` → drop
///   - `font-style`: `normal` → drop
///   - `font-weight`: `normal` → drop
///   - `font-variant`: `normal` → drop
///   - `font-stretch`: `normal` → drop
///   - `text-decoration`: `none` → drop
///   - `text-transform`: `none` → drop
pub fn simplify_styles(state: &mut ContentState) {
    let prunable = |name: &str, value: &str| -> bool {
        let v = value.trim();
        match name {
            "letter-spacing" | "word-spacing" => {
                matches!(v, "0" | "0em" | "0px" | "0rem" | "normal")
            }
            "text-indent" => matches!(v, "0" | "0em" | "0px" | "0rem" | "0%"),
            "white-space" | "font-style" | "font-weight" | "font-variant"
            | "font-stretch" => v == "normal",
            "text-decoration" | "text-transform" => v == "none",
            _ => false,
        }
    };
    let prune = |decl: &mut CssDecl| {
        decl.items.retain(|(k, v)| !prunable(k, v));
    };
    for v in state.stylesheet.values_mut() {
        prune(v);
    }
    for (_, decl) in &mut state.generated_classes {
        prune(decl);
    }
    for part in &mut state.book_parts {
        for decl in part.element_styles.values_mut() {
            prune(decl);
        }
    }
}

/// Dedupe per-element inline styles into auto-generated class rules.
///
/// Mirrors a subset of calibre's `fixup_styles_and_classes`
/// (yj_to_epub_properties.py:1388): when the same `style="..."` value
/// shows up on ≥ 2 elements across the book, promote it to a class
/// rule (`g<N>`) and replace the inline style with a class reference.
///
/// Runs BEFORE `finalize_chapter_attrs` so we can mutate
/// `element_styles` / `element_classes` instead of post-processing
/// already-rendered DOM attributes. Skipping this on horror is mostly
/// a no-op — only one inline-style string repeats (writing-mode:
/// horizontal-tb on figure captions) — but the same machinery is
/// what calibre uses to drive class-rule generation on books with
/// heavy inline overrides.
pub fn fixup_styles_and_classes(state: &mut ContentState) {
    // Build occurrence map: serialized inline style → count.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for part in &state.book_parts {
        for decl in part.element_styles.values() {
            if decl.is_empty() {
                continue;
            }
            *counts.entry(decl.to_inline()).or_insert(0) += 1;
        }
    }
    // Promote any style that appears ≥ 2 times. Calibre promotes all,
    // but for our purposes the small-count noise (1× one-off styles)
    // is better left inline — keeps the stylesheet readable.
    let mut promoted: HashMap<String, String> = HashMap::new();
    let mut sorted: Vec<(String, usize)> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (style_str, count) in sorted {
        if count < 2 {
            break; // Remaining entries are all single-occurrence.
        }
        let class_name = format!("g{}", state.generated_classes.len());
        // Rebuild a CssDecl from the serialized string so we can emit
        // the rule via the same path as named-style rules.
        let mut decl = CssDecl::new();
        for chunk in style_str.split(';') {
            let chunk = chunk.trim();
            if chunk.is_empty() {
                continue;
            }
            if let Some(colon) = chunk.find(':') {
                let k = chunk[..colon].trim();
                let v = chunk[colon + 1..].trim();
                decl.set(k, v);
            }
        }
        state.generated_classes.push((class_name.clone(), decl));
        promoted.insert(style_str, class_name);
    }
    if promoted.is_empty() {
        return;
    }
    // Rewrite: for any element whose serialized inline style matches a
    // promoted entry, replace the inline style with a class reference.
    for part in &mut state.book_parts {
        let style_keys: Vec<NodeId> = part.element_styles.keys().copied().collect();
        for id in style_keys {
            let style_str = part.element_styles[&id].to_inline();
            if let Some(class) = promoted.get(&style_str) {
                part.element_styles.remove(&id);
                part.element_classes
                    .entry(id)
                    .or_default()
                    .push(class.clone());
            }
        }
    }
}

pub fn finalize_chapter_attrs(state: &mut ContentState) {
    for part in &mut state.book_parts {
        // Apply class assignments.
        let classes_map: Vec<(NodeId, Vec<String>)> = part
            .element_classes
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        for (id, classes) in classes_map {
            if !classes.is_empty() {
                let joined = classes.join(" ");
                part.dom.get_mut(id).set("class", joined);
            }
        }
        // Apply inline styles.
        let styles_map: Vec<(NodeId, CssDecl)> = part
            .element_styles
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        for (id, decl) in styles_map {
            if !decl.is_empty() {
                part.dom.get_mut(id).set("style", decl.to_inline());
            }
        }
    }
}

/// Emit the final stylesheet from the deduplicated map.
///
/// Adds calibre-style `body { writing-mode: ...; }` derived from
/// `process_document_data`'s book-level writing-mode. Per the plan we do
/// NOT fabricate PPD from writing-mode (calibre does); they're emitted
/// independently when KFX declares them.
pub fn emit_stylesheet(state: &ContentState) -> String {
    let mut s = String::new();
    s.push_str("@charset \"utf-8\";\n");

    // Body defaults — writing-mode comes from document_data.
    if state.writing_mode != "horizontal-tb" {
        s.push_str(&format!(
            "body {{ writing-mode: {wm}; -webkit-writing-mode: {wm}; -epub-writing-mode: {wm}; }}\n",
            wm = state.writing_mode
        ));
    }

    let mut keys: Vec<&String> = state.stylesheet.keys().collect();
    keys.sort();
    for k in keys {
        let decl = &state.stylesheet[k];
        if decl.is_empty() {
            continue;
        }
        s.push_str(&format!(".s_{} {{ {} }}\n", safe_class_name(k), decl.to_inline()));
    }
    // Auto-generated classes from `fixup_styles_and_classes` (inline →
    // class dedupe). Emit in insertion order so class names stay stable
    // (g0, g1, ...) across runs on the same input.
    for (class_name, decl) in &state.generated_classes {
        if decl.is_empty() {
            continue;
        }
        s.push_str(&format!(".{} {{ {} }}\n", class_name, decl.to_inline()));
    }
    s
}
