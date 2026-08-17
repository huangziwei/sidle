//! KFX token stream for bidirectional conversion.
//!
//! The token stream is an intermediate representation that abstracts away
//! the nested Ion structure. Both import and export work through tokens:
//!
//! Import: Ion → TokenStream → IR
//! Export: IR → TokenStream → Ion
//!
//! ## Key Design: Generic Semantic Storage
//!
//! Tokens use `HashMap<SemanticTarget, String>` for semantic attributes,
//! not typed fields like `link_target` or `resource`. This keeps the token
//! layer format-agnostic - all format-specific logic lives in the schema.

use crate::formats::kfx::schema::SemanticTarget;
use crate::model::{NodeId, Role};
use std::collections::HashMap;

/// A token in the KFX content stream.
#[derive(Debug, Clone, PartialEq)]
pub enum KfxToken {
    /// Start of an element (container, paragraph, etc.)
    StartElement(ElementStart),
    /// End of an element
    EndElement,
    /// Text content
    Text(String),
    /// Start of an inline style span
    StartSpan(SpanStart),
    /// End of an inline style span
    EndSpan,
}

/// Information about an element start.
#[derive(Debug, Clone, PartialEq)]
pub struct ElementStart {
    /// The resolved IR role for this element.
    pub role: Role,
    /// Original IR node ID (for anchor creation during export).
    pub node_id: Option<NodeId>,
    /// KFX element ID (for anchors/links).
    pub id: Option<i64>,
    /// Semantic attributes (generic map, not typed fields).
    pub semantics: HashMap<SemanticTarget, String>,
    /// Content reference (for text lookup).
    pub content_ref: Option<ContentRef>,
    /// Inline style events (spans within text content).
    pub style_events: Vec<SpanStart>,
    /// Pre-transformed KFX attributes (field_id, value_string).
    /// Populated during export by schema.export_attributes().
    pub kfx_attrs: Vec<(u64, String)>,
    /// KFX style symbol ID for this element.
    /// Populated during export after registering the node's style.
    pub style_symbol: Option<u64>,
    /// Style name reference (for import lookup).
    /// Populated during import from the element's `style` field.
    pub style_name: Option<String>,
    /// Whether this element needs a container wrapper for borders to render.
    /// KFX requires block elements with borders to be `type: container` with
    /// nested `type: text` for content. Set during export by checking IR style.
    pub needs_container_wrapper: bool,
    /// Whether this element has block-level children (vs. only inline/text).
    /// Together with [`ElementStart::needs_container_wrapper`] this decides how a bordered
    /// element is emitted: a bordered *leaf* (inline content only) gets the
    /// inner-text wrapper; a bordered element *with block children* (e.g. a
    /// `罫囲み` `<div>` wrapping `<p>` lines) becomes a `type: container` whose
    /// children form the content list directly.
    pub has_block_children: bool,
    /// `layout` symbol for a bordered `type: container` — the block-progression
    /// axis of its children, keyed to the box's own (inheritance-resolved)
    /// writing mode: `horizontal` for vertical text (縦書き), `vertical` for
    /// horizontal-tb. A horizontally-typeset box inside a vertical book keeps
    /// `vertical`. `None` = not a container / fall back to the document axis.
    /// Set during export. See [`crate::formats::kfx::context::ExportContext::container_layout_symbol`].
    pub container_layout: Option<u64>,
    /// CSS declarations converted from the content element's own outer
    /// fields (as opposed to its named `$style` entity) — writing-mode
    /// resets, per-image sizing, etc. Populated during import; carried into
    /// the IR as the node's inline style.
    pub inline_style: Vec<(String, String)>,
    /// Whether the element declares `render: inline` (KFX `$601 = $283`) —
    /// an inline-flow replaced element (glyph image). Populated during
    /// import; inline images never get a block wrapper.
    pub render_inline: bool,
    /// Whether the KFX content type is `$271 image`. Populated during
    /// import. Role overrides (a `link_to` making the element a Link, a
    /// figure layout hint) must not swallow the `<img>` itself — the IR
    /// builder keys its image handling on this, not on `role`.
    pub is_image: bool,
    /// `$761 layout_hints` carried on the content element's own fields
    /// ("heading" / "figure" / "caption"). Populated during import; the IR
    /// builder merges these with the named style's hints to settle the
    /// element's role (calibre `attach_layout_hints` merge order).
    pub layout_hints: Vec<String>,
    /// `$790 yj.semantics.heading_level` from the element's own fields.
    pub heading_level: Option<String>,
    /// `$148 table_column_span` / `$149 table_row_span` — how many grid
    /// columns/rows this cell occupies. A cell carries no distinguishing
    /// element type in KFX (it is whatever `type` its content wants, sitting
    /// in a `table_row`'s content list), so the span rides on the element
    /// itself. Absent means one; both directions leave `None` alone.
    pub column_span: Option<u32>,
    pub row_span: Option<u32>,
    /// `$152 column_format` for a table: one entry per column. KFX states
    /// column geometry on the table rather than in the row structure, so on
    /// export the IR's `<colgroup>` collapses into this and emits no content
    /// element of its own. Empty for every other element.
    pub column_format: Vec<ColumnFormat>,
    /// `$104 list_start_offset` — the ordinal this list, or this item, counts
    /// from. A publisher's numbered list interrupted by prose arrives as one
    /// list per item, each stating where it resumes, so dropping the offset
    /// restarts every fragment at one.
    pub list_start: Option<u32>,
}

/// One column-geometry entry of a table's `column_format`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ColumnFormat {
    /// The column's geometry properties, already in KFX terms — in practice
    /// a `width` and its sizing box.
    pub fields: Vec<(u64, crate::formats::kfx::style_schema::KfxValue)>,
    /// `$118 column_span` — how many columns this entry describes.
    pub span: Option<u32>,
}

impl ElementStart {
    /// Create a new element start with just a role.
    pub fn new(role: Role) -> Self {
        Self {
            role,
            node_id: None,
            id: None,
            semantics: HashMap::new(),
            content_ref: None,
            style_events: Vec::new(),
            kfx_attrs: Vec::new(),
            style_symbol: None,
            style_name: None,
            needs_container_wrapper: false,
            has_block_children: false,
            container_layout: None,
            inline_style: Vec::new(),
            render_inline: false,
            is_image: false,
            layout_hints: Vec::new(),
            heading_level: None,
            column_span: None,
            row_span: None,
            column_format: Vec::new(),
            list_start: None,
        }
    }

    /// Get a semantic attribute value.
    pub fn get_semantic(&self, target: SemanticTarget) -> Option<&str> {
        self.semantics.get(&target).map(|s| s.as_str())
    }

    /// Set a semantic attribute value.
    pub fn set_semantic(&mut self, target: SemanticTarget, value: String) {
        self.semantics.insert(target, value);
    }
}

/// Reference to text in a content entity.
#[derive(Debug, Clone, PartialEq)]
pub struct ContentRef {
    pub name: String,
    pub index: usize,
}

/// Information about an inline span start.
///
/// The role and semantics are determined by the schema based on which fields are present.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanStart {
    /// IR Role determined by schema (Link, Inline, etc.)
    pub role: Role,
    /// Original IR node ID (for anchor creation during export).
    pub node_id: Option<NodeId>,
    /// Semantic attributes (generic map).
    pub semantics: HashMap<SemanticTarget, String>,
    /// Byte offset in parent text (for reconstruction).
    /// For import: populated from KFX style_event.
    /// For export: calculated during tokens_to_ion.
    pub offset: usize,
    /// Length in bytes.
    /// For import: populated from KFX style_event.
    /// For export: calculated during tokens_to_ion.
    pub length: usize,
    /// KFX style symbol ID (for export).
    /// Populated during ir_to_tokens from the node's registered style.
    pub style_symbol: Option<u64>,
    /// Pre-transformed KFX attributes (field_id, value_string).
    /// Populated during export by schema.export_attributes().
    pub kfx_attrs: Vec<(u64, String)>,
    /// Ruby annotation text resolved from a `ruby_name`+`ruby_id` style_event
    /// (import only; `role` is `Role::Ruby` when set). The IR builder appends a
    /// `Role::RubyText` child carrying this text when the Ruby span closes.
    pub ruby_annotation: Option<String>,
    /// Per-sub-run ruby pairs from a `ruby_id_list` style_event
    /// (import only): `(sub_offset, sub_length, annotation)` relative to the
    /// event's own offset. When non-empty the IR builder emits interleaved
    /// base-slice / `RubyText` pairs — calibre's grouped
    /// `<ruby><rb>…</rb><rt>…</rt><rb>…</rb><rt>…</rt></ruby>` shape —
    /// instead of the single-annotation form.
    pub ruby_pairs: Vec<(usize, usize, String)>,
}

impl SpanStart {
    /// Create a new span start.
    pub fn new(role: Role, offset: usize, length: usize) -> Self {
        Self {
            role,
            node_id: None,
            semantics: HashMap::new(),
            offset,
            length,
            style_symbol: None,
            kfx_attrs: Vec::new(),
            ruby_annotation: None,
            ruby_pairs: Vec::new(),
        }
    }

    /// Get a semantic attribute value.
    pub fn get_semantic(&self, target: SemanticTarget) -> Option<&str> {
        self.semantics.get(&target).map(|s| s.as_str())
    }

    /// Set a semantic attribute value.
    pub fn set_semantic(&mut self, target: SemanticTarget, value: String) {
        self.semantics.insert(target, value);
    }
}

/// A stream of KFX tokens with iterator support.
#[derive(Debug, Default)]
pub struct TokenStream {
    tokens: Vec<KfxToken>,
}

impl TokenStream {
    pub fn new() -> Self {
        Self { tokens: Vec::new() }
    }

    pub fn push(&mut self, token: KfxToken) {
        self.tokens.push(token);
    }

    pub fn start_element(&mut self, role: Role) {
        self.tokens
            .push(KfxToken::StartElement(ElementStart::new(role)));
    }

    pub fn start_element_with(
        &mut self,
        role: Role,
        id: Option<i64>,
        semantics: HashMap<SemanticTarget, String>,
        content_ref: Option<ContentRef>,
        style_events: Vec<SpanStart>,
    ) {
        self.tokens.push(KfxToken::StartElement(ElementStart {
            role,
            node_id: None,
            id,
            semantics,
            content_ref,
            style_events,
            kfx_attrs: Vec::new(),
            style_symbol: None,
            style_name: None,
            needs_container_wrapper: false,
            has_block_children: false,
            container_layout: None,
            inline_style: Vec::new(),
            render_inline: false,
            is_image: false,
            layout_hints: Vec::new(),
            heading_level: None,
            column_span: None,
            row_span: None,
            column_format: Vec::new(),
            list_start: None,
        }));
    }

    pub fn end_element(&mut self) {
        self.tokens.push(KfxToken::EndElement);
    }

    pub fn text(&mut self, s: impl Into<String>) {
        self.tokens.push(KfxToken::Text(s.into()));
    }

    pub fn start_span(&mut self, role: Role, semantics: HashMap<SemanticTarget, String>) {
        self.tokens.push(KfxToken::StartSpan(SpanStart {
            role,
            node_id: None,
            semantics,
            offset: 0,
            length: 0,
            style_symbol: None,
            kfx_attrs: Vec::new(),
            ruby_annotation: None,
            ruby_pairs: Vec::new(),
        }));
    }

    pub fn end_span(&mut self) {
        self.tokens.push(KfxToken::EndSpan);
    }

    pub fn iter(&self) -> impl Iterator<Item = &KfxToken> {
        self.tokens.iter()
    }

    #[allow(clippy::should_implement_trait)]
    pub fn into_iter(self) -> impl Iterator<Item = KfxToken> {
        self.tokens.into_iter()
    }

    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

impl IntoIterator for TokenStream {
    type Item = KfxToken;
    type IntoIter = std::vec::IntoIter<KfxToken>;

    fn into_iter(self) -> Self::IntoIter {
        self.tokens.into_iter()
    }
}

impl<'a> IntoIterator for &'a TokenStream {
    type Item = &'a KfxToken;
    type IntoIter = std::slice::Iter<'a, KfxToken>;

    fn into_iter(self) -> Self::IntoIter {
        self.tokens.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_stream_basic() {
        let mut stream = TokenStream::new();
        stream.start_element(Role::Paragraph);
        stream.text("Hello");
        stream.end_element();

        assert_eq!(stream.len(), 3);
    }

    #[test]
    fn test_token_stream_with_spans() {
        let mut stream = TokenStream::new();
        stream.start_element(Role::Paragraph);
        stream.text("Click ");

        let mut semantics = HashMap::new();
        semantics.insert(SemanticTarget::Href, "http://example.com".to_string());
        stream.start_span(Role::Link, semantics);
        stream.text("here");
        stream.end_span();
        stream.end_element();

        assert_eq!(stream.len(), 6);
    }

    #[test]
    fn test_element_semantics() {
        let mut elem = ElementStart::new(Role::Image);
        elem.set_semantic(SemanticTarget::Src, "cover.jpg".to_string());
        elem.set_semantic(SemanticTarget::Alt, "Cover image".to_string());

        assert_eq!(elem.get_semantic(SemanticTarget::Src), Some("cover.jpg"));
        assert_eq!(elem.get_semantic(SemanticTarget::Alt), Some("Cover image"));
        assert_eq!(elem.get_semantic(SemanticTarget::Href), None);
    }

    #[test]
    fn test_span_semantics() {
        let mut span = SpanStart::new(Role::Link, 10, 5);
        span.set_semantic(SemanticTarget::Href, "chapter2".to_string());

        assert_eq!(span.get_semantic(SemanticTarget::Href), Some("chapter2"));
        assert_eq!(span.offset, 10);
        assert_eq!(span.length, 5);
    }
}
