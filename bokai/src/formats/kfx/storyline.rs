//! KFX storyline parsing and IR building.
//!
//! This module handles bidirectional conversion between KFX storyline
//! structures and bokai's IR, using a schema-driven approach:
//!
//! Import: Ion → TokenStream → IR
//! Export: IR → TokenStream → Ion
//!
//! ## Key Design: Generic Interpreter
//!
//! The interpreter is completely generic - it knows nothing about KFX semantics.
//! All mapping logic is driven by the schema:
//!
//! 1. Read element type symbol ID
//! 2. Fetch Strategy from schema
//! 3. Execute Strategy to determine role
//! 4. Extract ALL attributes using schema's AttrRules
//! 5. Apply transformers to convert values

use crate::formats::kfx::anchor_table::AnchorTable;
use crate::formats::kfx::container::{SymbolTable, get_field};
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::schema::{SemanticTarget, schema};
use crate::formats::kfx::symbols::KfxSymbol;
use crate::formats::kfx::tokens::{
    ColumnFormat, ContentRef, ElementStart, KfxToken, SpanStart, TokenStream,
};
use crate::formats::kfx::transforms::ImportContext;
use crate::formats::kfx::yj_properties::{convert_yj_properties, partition_image_style};
use crate::model::{Chapter, Node, NodeId, Role};
use crate::style::CssDecl;
use std::collections::HashMap;

/// Context for tokenization including anchor resolution.
struct TokenizeContext<'a> {
    symbols: &'a SymbolTable,
    anchors: Option<&'a HashMap<String, String>>,
    /// Map of ruby_name (e.g. "b_ruby_0") → ordered list of annotation texts.
    /// Built from `ruby_content` entities by the importer. Style events with
    /// `ruby_name`+`ruby_id` resolve to `ruby_index[name][ruby_id - 1]`.
    ruby_index: Option<&'a HashMap<String, Vec<String>>>,
}

/// Shorthand for getting a KfxSymbol as u64.
macro_rules! sym {
    ($variant:ident) => {
        KfxSymbol::$variant as u64
    };
}

// ============================================================================
// IMPORT: Ion → TokenStream → IR
// ============================================================================

/// Tokenize a KFX storyline into a token stream.
///
/// This is the first stage of import: converting the nested Ion structure
/// into a flat stream of tokens that can be processed by the stack builder.
///
/// The `anchors` map resolves external links (anchor_name → uri).
/// The `styles` map passes through for the IR building phase.
pub fn tokenize_storyline(
    storyline: &IonValue,
    symbols: &SymbolTable,
    anchors: Option<&HashMap<String, String>>,
    _styles: Option<&HashMap<String, Vec<(u64, IonValue)>>>,
    ruby_index: Option<&HashMap<String, Vec<String>>>,
) -> TokenStream {
    let mut stream = TokenStream::new();

    let fields = match storyline.as_struct() {
        Some(f) => f,
        None => return stream,
    };

    let content_list = match get_field(fields, sym!(ContentList)) {
        Some(list) => list,
        None => return stream,
    };

    let ctx = TokenizeContext {
        symbols,
        anchors,
        ruby_index,
    };
    tokenize_content_list(content_list, &ctx, &mut stream);
    stream
}

/// Tokenize a content_list array.
fn tokenize_content_list(list: &IonValue, ctx: &TokenizeContext, stream: &mut TokenStream) {
    let items = match list.as_list() {
        Some(l) => l,
        None => return,
    };

    for item in items {
        tokenize_content_item(item, ctx, stream);
    }
}

/// Tokenize a single content item.
///
/// This is a **generic schema-driven interpreter** that:
/// 1. Reads the element's type symbol
/// 2. Looks up the strategy from the schema
/// 3. Executes the strategy to determine role
/// 4. Extracts ALL attributes using schema rules (no hardcoded targets)
/// 5. Applies transformers to values
fn tokenize_content_item(item: &IonValue, ctx: &TokenizeContext, stream: &mut TokenStream) {
    // Unwrap annotation if present
    let inner = item.unwrap_annotated();
    // A bare string is an inline text run: paragraphs that embed a replaced
    // element (`render: inline` images) carry their text as literal strings
    // in `content_list`, interleaved with the embedded structs.
    if let IonValue::String(text) = inner {
        stream.push(KfxToken::Text(text.clone()));
        return;
    }
    let fields = match inner.as_struct() {
        Some(f) => f,
        None => return,
    };

    // Get element type symbol ID (u64 from IonValue, cast to u32 for schema)
    let kfx_type_id = get_field(fields, sym!(Type))
        .and_then(|v| v.as_symbol())
        .unwrap_or(sym!(Container)) as u32;

    // Use schema to resolve role with attribute lookup closure
    // Return int values directly, or symbol IDs cast to i64 for symbol-based attributes
    let mut role = schema().resolve_element_role(kfx_type_id, |symbol| {
        get_field(fields, symbol as u64)
            .and_then(|v| v.as_int().or_else(|| v.as_symbol().map(|s| s as i64)))
    });

    // Check for semantic type annotation (yj.semantics.type) which uses local symbols.
    // The schema's StructureWithSemanticType strategies define what values map to what roles.
    if let Some(semantic_type) = get_semantic_type_annotation(fields, ctx.symbols)
        && let Some(mapped_role) = schema().role_for_semantic_type(&semantic_type)
    {
        role = mapped_role;
    }

    // Check for layout_hints to detect Figure and Caption elements (schema-driven).
    if let Some(layout_hints) = get_field(fields, sym!(LayoutHints))
        && let Some(hints_list) = layout_hints.as_list()
    {
        for hint in hints_list {
            if let Some(hint_id) = hint.as_symbol()
                && let Some(mapped_role) = schema().role_for_layout_hint(hint_id as u32)
            {
                role = mapped_role;
                break;
            }
        }
    }

    // Check for span indicators on elements (e.g., link_to → Link)
    // This enables standalone Link elements to be recognized
    if let Some(override_role) =
        schema().check_span_role_override(|sym| get_field(fields, sym as u64).is_some())
    {
        role = override_role;
    }

    // `$104 list_start_offset` — where this list, or this item, resumes
    // counting.
    let list_start = get_field(fields, sym!(ListStartOffset))
        .and_then(|v| v.as_int())
        .filter(|n| *n > 0)
        .map(|n| n as u32);

    // List tag parity with calibre's LIST_STYLE_TYPES: the five alpha/roman/
    // decimal styles make an `<ol>`, and every other style — KFX's own
    // `numeric` included — makes a `<ul>`.
    if matches!(role, Role::OrderedList | Role::UnorderedList) {
        let style = get_field(fields, sym!(ListStyle))
            .and_then(|v| ctx.symbols.text_of(v))
            .unwrap_or("");
        role = match style {
            "lower_alpha" | "upper_alpha" | "decimal" | "lower_roman" | "upper_roman" => {
                Role::OrderedList
            }
            // A stated offset is an ordinal, and only an ordered list can
            // carry one.
            _ if list_start.is_some() => Role::OrderedList,
            _ => Role::UnorderedList,
        };
    }

    // Get element ID
    let id = get_field(fields, sym!(Id)).and_then(|v| v.as_int());

    // Extract ALL semantic attributes using schema rules (GENERIC!)
    let mut semantics = extract_all_element_attrs(fields, kfx_type_id, ctx);

    // A note's body states which kind of note it is, the field a popup
    // presentation keys on.
    if let Some(epub_type) = get_field(fields, sym!(YjClassification))
        .and_then(|v| v.as_symbol())
        .and_then(note_epub_type)
    {
        semantics
            .entry(SemanticTarget::EpubType)
            .or_insert_with(|| epub_type.to_string());
    }

    // Get content reference (for text)
    let content_ref = get_field(fields, sym!(Content))
        .and_then(|v| v.as_struct())
        .and_then(|content_fields| {
            let name = get_field(content_fields, sym!(Name))
                .and_then(|v| resolve_symbol_or_string(v, ctx.symbols))?;
            let index = get_field(content_fields, sym!(Index))
                .and_then(|v| v.as_int())
                .map(|n| n as usize)?;
            Some(ContentRef { name, index })
        });

    // Get style events (inline spans) - fully schema-driven
    let style_events = get_field(fields, sym!(StyleEvents))
        .and_then(|v| v.as_list())
        .map(|events| parse_style_events(events, ctx))
        .unwrap_or_default();

    // Get nested children
    let has_children = get_field(fields, sym!(ContentList)).is_some();

    // Get style reference (symbol ID or name) for later lookup
    let style_name =
        get_field(fields, sym!(Style)).and_then(|v| resolve_symbol_or_string(v, ctx.symbols));

    // The element's own convertible properties (writing-mode resets,
    // per-image sizing, …) — the inline half of its styling, alongside the
    // named `$style` above.
    let inline_style =
        crate::formats::kfx::yj_properties::convert_yj_properties(fields, ctx.symbols).items;

    // `render: inline` — an inline-flow replaced element (glyph image).
    let render_inline = get_field(fields, sym!(Render))
        .and_then(|v| v.as_symbol())
        .is_some_and(|s| s == sym!(Inline));

    // Element-side `$761 layout_hints` / `$790 heading_level` — merged with
    // the named style's hints by the IR builder to settle the role.
    let (layout_hints, heading_level) =
        crate::formats::kfx::yj_properties::layout_hints_from_element_fields(fields);

    // Cell spans. A span of 1 is the absent case and carries nothing.
    let span_of = |field: u64| {
        get_field(fields, field)
            .and_then(|v| v.as_int())
            .filter(|n| *n > 1)
            .map(|n| n as u32)
    };
    let column_span = span_of(sym!(TableColumnSpan));
    let row_span = span_of(sym!(TableRowSpan));

    // Emit StartElement token
    stream.push(KfxToken::StartElement(ElementStart {
        role,
        node_id: None, // Only used during export
        id,
        semantics,
        content_ref,
        style_events,
        kfx_attrs: Vec::new(),
        style_symbol: None,             // Symbol ID (for export)
        style_name,                     // Style name (for import lookup)
        needs_container_wrapper: false, // Only used during export
        has_block_children: false,      // Only used during export
        container_layout: None,
        inline_style,
        render_inline,
        is_image: kfx_type_id == KfxSymbol::Image as u32,
        layout_hints,
        heading_level,
        column_span,
        row_span,
        column_format: Vec::new(),
        list_start,
    }));

    // A table's column geometry precedes its rows, as `<colgroup>` must.
    if role == Role::Table {
        tokenize_column_format(fields, ctx, stream);
    }

    // Recurse into children
    if has_children && let Some(children) = get_field(fields, sym!(ContentList)) {
        tokenize_content_list(children, ctx, stream);
    }

    // Emit EndElement token
    stream.push(KfxToken::EndElement);
}

/// Turn a table's `$152 column_format` list into a `<colgroup>` of `<col>`
/// entries.
///
/// This is the only statement a KFX table makes about its proportions. Each
/// entry describes one column — or `column_span` of them — and carries the
/// width as an ordinary length property, converting through the same
/// property table every other style uses. A list of bare `{is_empty: false}`
/// placeholders says nothing and produces nothing.
fn tokenize_column_format(
    fields: &[(u64, IonValue)],
    ctx: &TokenizeContext,
    stream: &mut TokenStream,
) {
    let Some(entries) = get_field(fields, sym!(ColumnFormat)).and_then(|v| v.as_list()) else {
        return;
    };
    // Every entry becomes a `<col>`, the ones saying nothing included:
    // columns are positional, and skipping a bare `{is_empty: false}` in the
    // middle slides every stated width one column to the left.
    let columns: Vec<ElementStart> = entries
        .iter()
        .map(|entry| {
            let mut col = ElementStart::new(Role::Column);
            let Some(entry_fields) = entry.unwrap_annotated().as_struct() else {
                return col;
            };
            col.inline_style = crate::formats::kfx::yj_properties::convert_yj_properties(
                entry_fields,
                ctx.symbols,
            )
            .items;
            col.column_span = get_field(entry_fields, sym!(ColumnSpan))
                .and_then(|v| v.as_int())
                .filter(|n| *n > 1)
                .map(|n| n as u32);
            col
        })
        .collect();
    // A list of pure placeholders states no geometry at all.
    if columns
        .iter()
        .all(|c| c.inline_style.is_empty() && c.column_span.is_none())
    {
        return;
    }
    stream.push(KfxToken::StartElement(ElementStart::new(Role::ColumnGroup)));
    for col in columns {
        stream.push(KfxToken::StartElement(col));
        stream.push(KfxToken::EndElement);
    }
    stream.push(KfxToken::EndElement);
}

/// The EPUB structural-semantics token for a KFX note classification.
///
/// `$615 yj.classification` marks a note's body, which is what lets a reader
/// show it in a popup when its reference is tapped. The three note kinds are
/// distinct — a single book uses all of them — and each keeps its own token,
/// and the mapping is the inverse of the one export applies.
fn note_epub_type(classification: u64) -> Option<&'static str> {
    match classification {
        x if x == KfxSymbol::Footnote as u64 => Some("footnote"),
        x if x == KfxSymbol::YjChapternote as u64 => Some("endnote"),
        x if x == KfxSymbol::YjEndnote as u64 => Some("rearnote"),
        x if x == KfxSymbol::YjSidenote as u64 => Some("sidebar"),
        _ => None,
    }
}

/// The KFX note classification for an `epub:type`, the inverse of
/// [`note_epub_type`].
///
/// An `epub:type` may carry several tokens; the most specific note kind in it
/// wins. Everything that names no note kind stays unclassified.
fn note_classification(epub_type: &str) -> Option<u64> {
    let tokens: Vec<&str> = epub_type.split_whitespace().collect();
    let has = |t: &str| tokens.contains(&t);
    if has("rearnote") {
        Some(KfxSymbol::YjEndnote as u64)
    } else if has("endnote") {
        Some(KfxSymbol::YjChapternote as u64)
    } else if has("sidebar") || has("marginalia") {
        Some(KfxSymbol::YjSidenote as u64)
    } else if has("footnote") {
        Some(KfxSymbol::Footnote as u64)
    } else {
        None
    }
}

/// Extract the semantic type annotation (yj.semantics.type) if present.
///
/// This looks for a field named "yj.semantics.type" (local symbol) and returns
/// its value as a string. Used for bidirectional BlockQuote mapping.
fn get_semantic_type_annotation(
    fields: &[(u64, IonValue)],
    symbols: &SymbolTable,
) -> Option<String> {
    // Find the field ID for "yj.semantics.type" in local symbols. Local
    // symbol ids start at the container's declared base, not at the static
    // table's length — `local_symbol_id` owns that offset.
    let field_id = symbols.local_symbol_id("yj.semantics.type")?;
    get_field(fields, field_id).and_then(|v| resolve_symbol_or_string(v, symbols))
}

/// Extract ALL semantic attributes for an element using schema rules.
///
/// This is **fully generic** - it iterates all AttrRules from the schema
/// and applies their transformers. Also checks span rules for attributes
/// like link_to that may appear on standalone elements.
fn extract_all_element_attrs(
    fields: &[(u64, IonValue)],
    kfx_type_id: u32,
    ctx: &TokenizeContext,
) -> HashMap<SemanticTarget, String> {
    let mut result = HashMap::new();
    let import_ctx = ImportContext {
        chapter_id: None,
        anchors: ctx.anchors,
    };

    // Extract using element attr rules
    for rule in schema().element_attr_rules(kfx_type_id) {
        if let Some(raw_value) = get_field(fields, rule.kfx_field as u64)
            .and_then(|v| resolve_symbol_or_string(v, ctx.symbols))
        {
            let parsed = rule.transform.import(&raw_value, &import_ctx);
            let final_value = match parsed {
                crate::formats::kfx::transforms::ParsedAttribute::String(s) => s,
                crate::formats::kfx::transforms::ParsedAttribute::Link(link) => link.to_href(),
                crate::formats::kfx::transforms::ParsedAttribute::Anchor(id) => id,
            };
            result.insert(rule.target, final_value);
        }
    }

    // Also extract using span rules (for attributes like link_to on standalone elements)
    let has_field = |symbol: KfxSymbol| get_field(fields, symbol as u64).is_some();
    for rule in schema().span_attr_rules(has_field) {
        // A `result` entry for this target wins.
        if result.contains_key(&rule.target) {
            continue;
        }
        if let Some(raw_value) = get_field(fields, rule.kfx_field as u64)
            .and_then(|v| resolve_symbol_or_string(v, ctx.symbols))
        {
            let parsed = rule.transform.import(&raw_value, &import_ctx);
            let final_value = match parsed {
                crate::formats::kfx::transforms::ParsedAttribute::String(s) => s,
                crate::formats::kfx::transforms::ParsedAttribute::Link(link) => link.to_href(),
                crate::formats::kfx::transforms::ParsedAttribute::Anchor(id) => id,
            };
            result.insert(rule.target, final_value);
        }
    }

    result
}

/// Parse style events from Ion using schema-driven interpretation.
fn parse_style_events(events: &[IonValue], ctx: &TokenizeContext) -> Vec<SpanStart> {
    events
        .iter()
        .filter_map(|event| {
            let fields = event.as_struct()?;
            let offset = get_field(fields, sym!(Offset))
                .and_then(|v| v.as_int())
                .map(|n| n as usize)?;
            let length = get_field(fields, sym!(Length))
                .and_then(|v| v.as_int())
                .map(|n| n as usize)?;

            // Get style symbol ID for later lookup
            let style_symbol = get_field(fields, sym!(Style)).and_then(|v| v.as_symbol());

            // Ruby span: a style_event with `ruby_name` points at an entry in
            // a `ruby_content` fragment, resolved here to the annotation text
            // the IR builder attaches as an `<rt>` child on close.
            if let Some(ruby_name) = get_field(fields, sym!(RubyName))
                .and_then(|v| resolve_symbol_or_string(v, ctx.symbols))
            {
                let ruby_id_1 = get_field(fields, sym!(RubyId))
                    .and_then(|v| v.as_int())
                    .map(|n| n as usize)
                    .unwrap_or(0);
                // An annotation lookup always resolves to `Some`: a miss
                // yields "" and an empty `<rt>`.
                let annotation_for = |id_1: usize| -> String {
                    ctx.ruby_index
                        .and_then(|idx| idx.get(&ruby_name))
                        .and_then(|annotations| annotations.get(id_1.saturating_sub(1)))
                        .cloned()
                        .unwrap_or_default()
                };
                // `ruby_id_list`: per-sub-run pairs (grouped ruby covering
                // several base segments, each with its own annotation).
                let ruby_pairs: Vec<(usize, usize, String)> =
                    match get_field(fields, sym!(RubyIdList)).and_then(|v| v.as_list()) {
                        Some(list) => list
                            .iter()
                            .filter_map(|entry| {
                                let f = entry.unwrap_annotated().as_struct()?;
                                let o = get_field(f, sym!(Offset))?.as_int()? as usize;
                                let l = get_field(f, sym!(Length))?.as_int()? as usize;
                                let id_1 = get_field(f, sym!(RubyId))?.as_int()? as usize;
                                Some((o, l, annotation_for(id_1)))
                            })
                            .collect(),
                        None => Vec::new(),
                    };
                let ruby_annotation = ruby_pairs.is_empty().then(|| annotation_for(ruby_id_1));
                return Some(SpanStart {
                    role: Role::Ruby,
                    node_id: None,
                    semantics: HashMap::new(),
                    offset,
                    length,
                    style_symbol,
                    kfx_attrs: Vec::new(),
                    ruby_annotation,
                    ruby_pairs,
                });
            }

            // Create closure to check which fields are present
            let has_field = |symbol: KfxSymbol| get_field(fields, symbol as u64).is_some();

            // Use schema to determine role
            let role = schema().resolve_span_role(has_field);

            // Extract ALL semantic attributes using schema rules (GENERIC!)
            let semantics = extract_all_span_attrs(fields, has_field, ctx);

            Some(SpanStart {
                role,
                node_id: None, // Only used during export
                semantics,
                offset,
                length,
                style_symbol,
                kfx_attrs: Vec::new(),
                ruby_annotation: None,
                ruby_pairs: Vec::new(),
            })
        })
        .collect()
}

/// Extract ALL semantic attributes for a span using schema rules.
///
/// This is **fully generic** - no hardcoded SemanticTarget checks.
fn extract_all_span_attrs<F>(
    fields: &[(u64, IonValue)],
    has_field: F,
    ctx: &TokenizeContext,
) -> HashMap<SemanticTarget, String>
where
    F: Fn(KfxSymbol) -> bool,
{
    let mut result = HashMap::new();
    let import_ctx = ImportContext {
        chapter_id: None,
        anchors: ctx.anchors,
    };

    for rule in schema().span_attr_rules(&has_field) {
        if let Some(raw_value) = get_field(fields, rule.kfx_field as u64)
            .and_then(|v| resolve_symbol_or_string(v, ctx.symbols))
        {
            // Apply the transformer to convert the raw value
            let parsed = rule.transform.import(&raw_value, &import_ctx);

            // Convert ParsedAttribute to string for storage
            let final_value = match parsed {
                crate::formats::kfx::transforms::ParsedAttribute::String(s) => s,
                crate::formats::kfx::transforms::ParsedAttribute::Link(link) => link.to_href(),
                crate::formats::kfx::transforms::ParsedAttribute::Anchor(id) => id,
            };

            result.insert(rule.target, final_value);
        }
    }

    result
}

// ============================================================================
// Token Stream → IR (Stack-based builder)
// ============================================================================

/// Pending style_events of an open interleave element — one whose text
/// arrives as bare-string runs in `content_list` (mixed with child elements)
/// with no content ref. `cursor` tracks the position in the element's
/// event offset space: runs advance it by their char count, each direct
/// child element by exactly ONE position regardless of its own length
/// (Amazon's counting — same rule the fidelity extractor and the epub→kfx
/// emitter use for in-run images).
struct InterleaveEvents {
    events: Vec<SpanStart>,
    cursor: usize,
}

/// Clamp `events` to the run occupying `[start, end)` of the parent's event
/// offset space, rebasing offsets to be run-local. Events without overlap
/// drop; an event crossing the run boundary (its range covers a child
/// element) is cut at the boundary, keeping only the ruby sub-pairs that fit
/// inside the kept slice.
fn clamp_events_to_run(events: &[SpanStart], start: usize, end: usize) -> Vec<SpanStart> {
    let mut out = Vec::new();
    for e in events {
        let e_start = e.offset;
        let e_end = e.offset + e.length;
        let o_start = e_start.max(start);
        let o_end = e_end.min(end);
        if o_start >= o_end {
            continue;
        }
        let mut clamped = e.clone();
        clamped.offset = o_start - start;
        clamped.length = o_end - o_start;
        clamped.ruby_pairs = e
            .ruby_pairs
            .iter()
            .filter(|(sub_off, sub_len, _)| {
                let abs = e_start + sub_off;
                abs >= o_start && abs + sub_len <= o_end
            })
            .map(|(sub_off, sub_len, ann)| (e_start + sub_off - o_start, *sub_len, ann.clone()))
            .collect();
        out.push(clamped);
    }
    out
}

/// Build an IR chapter from a token stream.
///
/// Uses a stack-based approach to handle nested elements.
/// Applies semantics **generically** from the token's semantics map.
/// The `styles` map looks up style definitions by name.
/// The `symbols` table resolves style symbol IDs to names.
pub fn build_ir_from_tokens<F>(
    tokens: &TokenStream,
    symbols: &SymbolTable,
    styles: Option<&HashMap<String, Vec<(u64, IonValue)>>>,
    content_lookup: F,
) -> Chapter
where
    F: FnMut(&str, usize) -> Option<String>,
{
    build_ir_from_tokens_anchored(tokens, symbols, styles, None, content_lookup)
}

/// [`build_ir_from_tokens`] with anchor stamping: elements at anchored
/// `(eid, 0)` positions get their `semantics.id` from the anchor table (the
/// same `a85J` / `toc-148-0` names calibre stamps), and
/// registered offsets > 0 are located inside the element's text and marked
/// with a zero-length anchor span. Elements' raw eids are never emitted —
/// shipped EPUBs carry only anchor-backed ids.
pub fn build_ir_from_tokens_anchored<F>(
    tokens: &TokenStream,
    symbols: &SymbolTable,
    styles: Option<&HashMap<String, Vec<(u64, IonValue)>>>,
    anchor_table: Option<&AnchorTable>,
    mut content_lookup: F,
) -> Chapter
where
    F: FnMut(&str, usize) -> Option<String>,
{
    let mut chapter = Chapter::new();
    // (node, pending render-inline demotion, pending interleave events). The
    // demotion check runs at element close, over the finished subtree;
    // interleave events apply to each bare-string run as it arrives.
    let mut stack: Vec<(NodeId, bool, Option<InterleaveEvents>)> =
        vec![(chapter.root(), false, None)];
    // Every element's declared eid, in document order — the offset-anchor
    // pass below locates offsets inside these elements' finished subtrees.
    let mut eid_nodes: Vec<(i64, NodeId)> = Vec::new();
    // Elements whose text arrived as interleave runs: their offset anchors
    // resolve in event space (see `locate_offset_event_space`).
    let mut interleave_roots: std::collections::HashSet<NodeId> = std::collections::HashSet::new();

    for token in tokens {
        match token {
            KfxToken::StartElement(elem) => {
                // A child element occupies exactly ONE position in an
                // enclosing interleave element's event offset space,
                // regardless of its own content length.
                if let Some((_, _, Some(iv))) = stack.last_mut() {
                    iv.cursor += 1;
                }
                let parent = stack
                    .last()
                    .map(|(n, _, _)| *n)
                    .unwrap_or_else(|| chapter.root());

                // Create the node. Image-typed elements always emit an
                // `<img>` node — a role override (a `link_to` making the
                // element a Link, a figure layout hint) must not swallow it.
                let node_role = if elem.is_image {
                    Role::Image
                } else {
                    resolve_hinted_role(elem, styles, symbols, anchor_table)
                };
                let node_id = chapter.alloc_node(Node::new(node_role));

                // An image element's `link_to` becomes an `<a>` wrapping the
                // whole emitted structure (calibre wraps the
                // returned element the same way).
                let linked_image_href = elem
                    .is_image
                    .then(|| elem.semantics.get(&SemanticTarget::Href))
                    .flatten();
                let attach_parent = match linked_image_href {
                    Some(href) => {
                        let a = chapter.alloc_node(Node::new(Role::Link));
                        chapter.append_child(parent, a);
                        chapter.semantics.set_href(a, href);
                        a
                    }
                    None => parent,
                };

                // A block image takes calibre's wrapper partition: `$style`
                // and the element's own properties merge, the block-level half
                // moves onto a wrapper `<div>`, and both halves go inline.
                let mut wrap: Option<(CssDecl, CssDecl)> = None;
                if elem.is_image && !elem.render_inline {
                    let mut merged = CssDecl::new();
                    if let Some(style_name) = &elem.style_name
                        && let Some(styles_map) = styles
                        && let Some(kfx_props) = styles_map.get(style_name)
                    {
                        for (k, v) in convert_yj_properties(kfx_props, symbols).items {
                            merged.set(k, v);
                        }
                    }
                    for (k, v) in &elem.inline_style {
                        merged.set(k.clone(), v.clone());
                    }
                    wrap = partition_image_style(merged);
                }

                // Anchors/eids stamp onto the wrapper when one exists
                // (calibre's `move_anchor`).
                let anchor_node;
                if let Some((wrapper_decl, img_decl)) = wrap {
                    // The wrapper carries the image element's layout hints: a
                    // heading/figure/caption-hinted block image promotes the
                    // wrapper `<div>` to `<hN>` / `<figure>` / `<figcaption>`.
                    let wrapper_role = hinted_wrapper_role(elem, styles, symbols, anchor_table)
                        .unwrap_or(Role::Container);
                    let wrapper = chapter.alloc_node(Node::new(wrapper_role));
                    chapter.append_child(attach_parent, wrapper);
                    chapter.append_child(wrapper, node_id);
                    if !wrapper_decl.is_empty() {
                        chapter
                            .semantics
                            .set_style(wrapper, &wrapper_decl.to_inline());
                    }
                    if !img_decl.is_empty() {
                        chapter.semantics.set_style(node_id, &img_decl.to_inline());
                    }
                    anchor_node = wrapper;
                } else {
                    chapter.append_child(attach_parent, node_id);
                    // Apply the style the styles map holds. The raw KFX style
                    // name stays as the node's source class identity, which
                    // normalized export names its stylesheet rules after.
                    if let Some(style_name) = &elem.style_name {
                        chapter.semantics.set_class(node_id, style_name);
                        if let Some(styles_map) = styles
                            && let Some(kfx_props) = styles_map.get(style_name)
                        {
                            let ir_style = kfx_style_to_ir(kfx_props, symbols);
                            let style_id = chapter.styles.intern(ir_style);
                            if let Some(node) = chapter.node_mut(node_id) {
                                node.style = style_id;
                            }
                        }
                    }
                    if !elem.inline_style.is_empty() {
                        let mut decl = CssDecl::new();
                        for (k, v) in &elem.inline_style {
                            decl.set(k.clone(), v.clone());
                        }
                        chapter.semantics.set_style(node_id, &decl.to_inline());
                    }
                    anchor_node = node_id;
                }

                // Apply every semantic attribute from the generic map, minus
                // the href a linked image's `<a>` wrapper consumed: an
                // `<img href>` is invalid.
                if linked_image_href.is_some() {
                    let mut rest = elem.semantics.clone();
                    rest.remove(&SemanticTarget::Href);
                    apply_semantics_to_node(&mut chapter, node_id, &rest);
                } else {
                    apply_semantics_to_node(&mut chapter, node_id, &elem.semantics);
                }

                // Cell spans belong to the cell element, not to a link wrapper
                // standing in front of it. Amazon states them in the cell's
                // named style, and the element field is read too.
                let (style_cols, style_rows) = elem
                    .style_name
                    .as_ref()
                    .and_then(|name| styles?.get(name))
                    .map(|props| cell_spans_from_style(props))
                    .unwrap_or_default();
                if let Some(n) = elem.column_span.or(style_cols) {
                    chapter.semantics.set_col_span(node_id, n);
                }
                if let Some(n) = elem.row_span.or(style_rows) {
                    chapter.semantics.set_row_span(node_id, n);
                }

                if let Some(n) = elem.list_start {
                    chapter.semantics.set_list_start(node_id, n);
                }

                // Anchored element: stamp the html id registered at
                // `(eid, 0)`, never the raw eid.
                if let Some(eid) = elem.id {
                    eid_nodes.push((eid, anchor_node));
                    // The eid itself rides on the node as source identity,
                    // separate from any emitted html id. A renderer needs it to
                    // mark up the element a stored `(eid, offset)` handle names.
                    chapter.semantics.set_source_element(anchor_node, eid);
                    if let Some(table) = anchor_table
                        && chapter.semantics.id(anchor_node).is_none()
                        && let Some(anchor_id) = table.id_at(eid, 0)
                    {
                        chapter.semantics.set_id(anchor_node, &anchor_id);
                    }
                }

                // Handle text content with style events
                if let Some(ref content_ref) = elem.content_ref
                    && let Some(text) = content_lookup(&content_ref.name, content_ref.index)
                {
                    if elem.style_events.is_empty() {
                        // Simple case: no inline styles
                        let range = chapter.append_text(&text);
                        let text_node = chapter.alloc_node(Node::text(range));
                        chapter.append_child(node_id, text_node);
                    } else {
                        // Complex case: apply style events as spans
                        build_text_with_spans(
                            &mut chapter,
                            node_id,
                            &text,
                            &elem.style_events,
                            symbols,
                            styles,
                        );
                    }
                }

                // With no content ref the element's text arrives as bare-string
                // runs in content_list, interleaved with child elements. Its
                // style_events offset into that joined space.
                let interleave = (elem.content_ref.is_none() && !elem.style_events.is_empty())
                    .then(|| InterleaveEvents {
                        events: elem.style_events.clone(),
                        cursor: 0,
                    });
                if interleave.is_some() {
                    interleave_roots.insert(node_id);
                }

                stack.push((node_id, elem.render_inline && !elem.is_image, interleave));
            }

            KfxToken::EndElement => {
                if let Some((node_id, demote, _)) = stack.pop()
                    && demote
                {
                    // A `render: inline` block container demotes to a span when
                    // every descendant is inline-only. A block child, or a
                    // forced line break, keeps the block box.
                    let all_inline = chapter
                        .children(node_id)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .all(|c| is_inline_only_ir(&chapter, c));
                    if all_inline
                        && matches!(
                            chapter.node(node_id).map(|n| n.role),
                            Some(Role::Paragraph | Role::Container)
                        )
                    {
                        if let Some(n) = chapter.node_mut(node_id) {
                            n.role = Role::Inline;
                        }
                        chapter.semantics.set_render_inline(node_id);
                    }
                }
            }

            KfxToken::Text(text) => {
                // A bare run inside an interleave element takes the slice of
                // the parent's style_events overlapping it (ruby, links,
                // styled spans), then advances the cursor.
                let mut run_events: Option<Vec<SpanStart>> = None;
                let parent = match stack.last_mut() {
                    Some((n, _, Some(iv))) => {
                        let run_len = text.chars().count();
                        run_events = Some(clamp_events_to_run(
                            &iv.events,
                            iv.cursor,
                            iv.cursor + run_len,
                        ));
                        iv.cursor += run_len;
                        *n
                    }
                    Some((n, _, None)) => *n,
                    None => chapter.root(),
                };
                match run_events {
                    Some(events) if !events.is_empty() => {
                        build_text_with_spans(&mut chapter, parent, text, &events, symbols, styles);
                    }
                    _ => {
                        let range = chapter.append_text(text);
                        let text_node = chapter.alloc_node(Node::text(range));
                        chapter.append_child(parent, text_node);
                    }
                }
            }

            KfxToken::StartSpan(_) | KfxToken::EndSpan => {
                // Style events are handled via ElementStart.style_events
            }
        }
    }

    // Offset anchors (`(eid, offset > 0)`): locate each offset inside the
    // element's finished subtree and stamp a zero-length span there, which
    // lands a mid-paragraph nav target (`…#page-911-2`) on its exact position.
    if let Some(table) = anchor_table {
        stamp_offset_anchors(&mut chapter, &eid_nodes, table, &interleave_roots);
    }

    chapter
}

/// The IR analog of calibre's `is_inline_only` (calibre
/// `yj_to_epub_content.py`): inline elements with every descendant
/// inline-only. A text run containing `\n` counts as NOT inline — calibre's
/// DOM materializes the break as a `<br>` child, which is outside calibre's
/// inline tag set and blocks the demotion there.
fn is_inline_only_ir(chapter: &Chapter, node: NodeId) -> bool {
    let Some(n) = chapter.node(node) else {
        return false;
    };
    match n.role {
        Role::Text => !chapter.text(n.text).contains('\n'),
        Role::Link | Role::Image | Role::Ruby | Role::RubyText | Role::Inline => chapter
            .children(node)
            .collect::<Vec<_>>()
            .into_iter()
            .all(|c| is_inline_only_ir(chapter, c)),
        _ => false,
    }
}

/// Settle a block element's role from the merged `$761 layout_hints` and
/// `$790 heading_level` channels, in calibre's `attach_layout_hints`
/// precedence: the named style's hints and level, the element's own fields
/// merged after with the level filling only when unset, and the anchor table's
/// nav-registered heading level as the last fallback. Default level 1.
///
/// Promotion to a heading takes the "heading" hint. A bare `$790` level
/// promotes nothing, and a schema-assigned `Heading` role with no heading hint
/// settles back to `Paragraph`. Figure and caption hints settle the role the
/// same way, and their presence blocks the bare-div collapse.
fn resolve_hinted_role(
    elem: &ElementStart,
    styles: Option<&HashMap<String, Vec<(u64, IonValue)>>>,
    symbols: &SymbolTable,
    anchor_table: Option<&AnchorTable>,
) -> Role {
    let role = elem.role;
    let (hints, level) = merged_layout_hints(elem, styles, symbols);
    if hints.is_empty() && level.is_none() && !matches!(role, Role::Heading(_)) {
        return role;
    }
    if hints.iter().any(|h| h == "heading") {
        if matches!(
            role,
            Role::Paragraph | Role::Container | Role::Heading(_) | Role::BlockQuote
        ) {
            return Role::Heading(resolve_heading_level(level, elem.id, anchor_table));
        }
        return role;
    }
    if let Role::Heading(_) = role {
        // `$790` without a "heading" hint anywhere: calibre
        // never promotes these.
        return Role::Paragraph;
    }
    if matches!(role, Role::Paragraph | Role::Container) {
        if hints.iter().any(|h| h == "figure") {
            return Role::Figure;
        }
        if hints.iter().any(|h| h == "caption") {
            return Role::Caption;
        }
    }
    role
}

/// Merge the element's layout hints / heading level: the named style's
/// hints/level first (calibre inserts these), the element's own fields
/// after (level fills only when unset) — calibre's
/// `attach_layout_hints` precedence.
fn merged_layout_hints(
    elem: &ElementStart,
    styles: Option<&HashMap<String, Vec<(u64, IonValue)>>>,
    symbols: &SymbolTable,
) -> (Vec<String>, Option<String>) {
    let (mut hints, mut level) = match elem
        .style_name
        .as_ref()
        .and_then(|name| styles.and_then(|m| m.get(name)))
    {
        Some(props) => {
            crate::formats::kfx::yj_properties::style_fields_layout_hints(props, symbols)
        }
        None => (Vec::new(), None),
    };
    for h in &elem.layout_hints {
        if !hints.iter().any(|existing| existing == h) {
            hints.push(h.clone());
        }
    }
    if level.is_none() {
        level = elem.heading_level.clone();
    }
    (hints, level)
}

/// Resolve a heading level: the explicit `$790` level, else the anchor
/// table's nav-registered level at `(eid, 0)` (calibre `process_position`),
/// else 1.
fn resolve_heading_level(
    level: Option<String>,
    eid: Option<i64>,
    anchor_table: Option<&AnchorTable>,
) -> u8 {
    let anchor_level = anchor_table
        .zip(eid)
        .and_then(|(t, eid)| t.heading_level_at(eid, 0))
        .map(|l| l.to_string());
    level
        .or(anchor_level)
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(1)
}

/// The block role a wrapped block image's `<div>` wrapper should take from
/// the image element's merged layout hints — calibre attaches
/// the hints to the wrapper (`wrap_block_image` → `attach_layout_hints`),
/// which `consolidate_html` then promotes (a heading whose content is a
/// single block image becomes `<hN>`; an `<img>` is not a block descendant,
/// and the promotion fires). `None` keeps the plain `Container`.
fn hinted_wrapper_role(
    elem: &ElementStart,
    styles: Option<&HashMap<String, Vec<(u64, IonValue)>>>,
    symbols: &SymbolTable,
    anchor_table: Option<&AnchorTable>,
) -> Option<Role> {
    let (hints, level) = merged_layout_hints(elem, styles, symbols);
    if hints.iter().any(|h| h == "heading") {
        Some(Role::Heading(resolve_heading_level(
            level,
            elem.id,
            anchor_table,
        )))
    } else if hints.iter().any(|h| h == "figure") {
        Some(Role::Figure)
    } else if hints.iter().any(|h| h == "caption") {
        Some(Role::Caption)
    } else {
        None
    }
}

/// The main page template's identity within one section: its own `$155` id,
/// `$157 style` name, and the CSS declarations converted from its remaining
/// outer fields (a per-section `writing_mode` is the common one), carried
/// onto the chapter's root container.
#[derive(Debug, Default, Clone)]
pub struct SectionTemplate {
    pub eid: Option<i64>,
    pub style: Option<String>,
    pub inline_style: Vec<(String, String)>,
}

/// Re-root the chapter under the section's main page-template container —
/// the body-level `<div>` calibre emits for every reflowable
/// section — carrying the template's style, then stamp the anchors reaching
/// this level: the template's `(eid, 0)` first, then the storyline root's
/// (calibre's `process_section` → `process_story` order; first id wins). The
/// template's offsets > 0 locate across the whole chapter text; a storyline
/// root never walks offsets (calibre stamps it at offset 0 only).
pub fn apply_section_template(
    chapter: &mut Chapter,
    template: &SectionTemplate,
    story_eid: Option<i64>,
    symbols: &SymbolTable,
    styles: Option<&HashMap<String, Vec<(u64, IonValue)>>>,
    anchor_table: Option<&AnchorTable>,
) {
    let SectionTemplate {
        eid: template_eid,
        style: template_style,
        inline_style: template_inline,
    } = template;
    let (template_eid, template_style) = (*template_eid, template_style.as_deref());
    let root = chapter.root();
    let wrapper = chapter.alloc_node(Node::new(Role::Container));

    if let Some(style_name) = template_style {
        chapter.semantics.set_class(wrapper, style_name);
        if let Some(styles_map) = styles
            && let Some(kfx_props) = styles_map.get(style_name)
        {
            let ir_style = kfx_style_to_ir(kfx_props, symbols);
            let style_id = chapter.styles.intern(ir_style);
            if let Some(node) = chapter.node_mut(wrapper) {
                node.style = style_id;
            }
        }
    }
    if !template_inline.is_empty() {
        let mut decl = CssDecl::new();
        for (k, v) in template_inline {
            decl.set(k.clone(), v.clone());
        }
        chapter.semantics.set_style(wrapper, &decl.to_inline());
    }

    // Re-parent: the root's children become the wrapper's, the wrapper
    // becomes the root's only child.
    let children: Vec<NodeId> = chapter.children(root).collect();
    if let Some(w) = chapter.node_mut(wrapper) {
        w.first_child = children.first().copied();
    }
    for child in &children {
        if let Some(c) = chapter.node_mut(*child) {
            c.parent = Some(wrapper);
        }
    }
    if let Some(r) = chapter.node_mut(root) {
        r.first_child = None;
    }
    chapter.append_child(root, wrapper);

    // The template container is itself an addressable element: it carries a
    // `$155 id` for device handles and the position map to name, and it
    // leads the chapter's element order.
    if let Some(eid) = template_eid {
        chapter.semantics.set_source_element(wrapper, eid);
    }

    let Some(table) = anchor_table else {
        return;
    };
    for eid in [template_eid, story_eid].into_iter().flatten() {
        if chapter.semantics.id(wrapper).is_none()
            && let Some(anchor_id) = table.id_at(eid, 0)
        {
            chapter.semantics.set_id(wrapper, &anchor_id);
        }
    }
    if let Some(eid) = template_eid {
        // The template wrapper is a container, never an interleave element —
        // its offsets walk calibre's DOM space.
        stamp_offset_anchors(chapter, &[(eid, wrapper)], table, &Default::default());
    }
}

/// Stamp every registered `(eid, offset > 0)` anchor into the built chapter.
/// Offsets ascend per element; earlier stamps insert only zero-length nodes,
/// and later locates count the same character stream.
fn stamp_offset_anchors(
    chapter: &mut Chapter,
    eid_nodes: &[(i64, NodeId)],
    table: &AnchorTable,
    interleave_roots: &std::collections::HashSet<NodeId>,
) {
    for &(eid, elem) in eid_nodes {
        let event_space = interleave_roots.contains(&elem);
        for off in table.offsets_beyond_zero(eid) {
            let Some(anchor_id) = table.id_at(eid, off) else {
                continue;
            };
            let target = if event_space {
                locate_offset_event_space(chapter, elem, off)
            } else {
                locate_offset_ir(chapter, elem, off)
            };
            if let Some(target) = target
                && chapter.semantics.id(target).is_none()
            {
                chapter.semantics.set_id(target, &anchor_id);
            }
        }
    }
}

/// Outcome of an offset walk below one node: the located anchor target, or
/// the unconsumed remaining offset.
enum Located {
    Found(NodeId),
    Remaining(i64),
}

/// Locate `offset` code points into `root`'s text and return the node to
/// stamp — the IR analog of calibre's `locate_offset` (calibre
/// `yj_to_epub_content.py`, `split_after=false, zero_len=true`): a
/// mid-text offset splits the text run around a fresh zero-length span, an
/// offset at the very end appends one to the element, an offset past the text
/// stamps nothing.
fn locate_offset_ir(chapter: &mut Chapter, root: NodeId, offset: i64) -> Option<NodeId> {
    let children: Vec<NodeId> = chapter.children(root).collect();
    let mut remaining = offset;
    for child in children {
        match locate_offset_in_ir(chapter, child, remaining) {
            Located::Found(n) => return Some(n),
            Located::Remaining(r) => remaining = r,
        }
    }
    if remaining == 0 {
        // End-of-text position: fresh zero-length span as the element's last
        // child (calibre's `SubElement(root, "span")`).
        let span = chapter.alloc_node(Node::new(Role::Inline));
        chapter.append_child(root, span);
        return Some(span);
    }
    None
}

/// [`locate_offset_ir`] for interleave-built elements, counting in the
/// element's own KFX event offset space, not calibre's DOM-walk space, which
/// reconstructs no mark over interleave content. In event space every bare-run
/// code point counts (`\n` included), each content_list child element counts
/// exactly one position, ruby base text counts while annotation text does not,
/// and event-span wrappers are transparent.
fn locate_offset_event_space(chapter: &mut Chapter, root: NodeId, offset: i64) -> Option<NodeId> {
    let children: Vec<NodeId> = chapter.children(root).collect();
    let mut remaining = offset;
    for child in children {
        match locate_offset_in_event_space(chapter, child, remaining) {
            Located::Found(n) => return Some(n),
            Located::Remaining(r) => remaining = r,
        }
    }
    if remaining == 0 {
        let span = chapter.alloc_node(Node::new(Role::Inline));
        chapter.append_child(root, span);
        return Some(span);
    }
    None
}

/// One node's contribution to the event-space walk (see
/// [`locate_offset_event_space`]).
fn locate_offset_in_event_space(chapter: &mut Chapter, node: NodeId, offset: i64) -> Located {
    if offset < 0 {
        return Located::Remaining(offset);
    }
    let Some(n) = chapter.node(node) else {
        return Located::Remaining(offset);
    };
    match n.role {
        Role::Text => {
            let countable = chapter.text(n.text).chars().count() as i64;
            if countable > 0 {
                if offset == 0 {
                    return Located::Found(wrap_text_run_in_span(chapter, node));
                }
                if offset < countable {
                    return Located::Found(split_text_run(chapter, node, offset));
                }
            }
            Located::Remaining(offset - countable)
        }
        // Annotation text is invisible in event space — events offset over
        // the base text the annotations decorate.
        Role::RubyText => Located::Remaining(offset),
        // Ruby wrappers and event spans are decoration over the parent's own
        // runs: descend, their text counts through.
        Role::Ruby | Role::Link => descend_event_space(chapter, node, offset),
        Role::Inline => {
            if chapter.semantics.render_inline(node) {
                // A demoted content_list child element (tate-chu-yoko run):
                // one opaque position.
                if offset == 0 {
                    return Located::Found(node);
                }
                Located::Remaining(offset - 1)
            } else if offset == 0
                && chapter.children(node).next().is_some_and(|first| {
                    chapter
                        .node(first)
                        .is_some_and(|f| f.role == Role::Text && !chapter.text(f.text).is_empty())
                })
            {
                // An event span whose text starts exactly at the offset carries
                // the id itself, on calibre's rule minus its `\n` invisibility:
                // event space counts every code point.
                Located::Found(node)
            } else {
                descend_event_space(chapter, node, offset)
            }
        }
        // Any other role is a content_list child element (image, nested
        // container): one opaque position.
        _ => {
            if offset == 0 {
                return Located::Found(node);
            }
            Located::Remaining(offset - 1)
        }
    }
}

/// Walk `node`'s children in document order in event space.
fn descend_event_space(chapter: &mut Chapter, node: NodeId, mut offset: i64) -> Located {
    let children: Vec<NodeId> = chapter.children(node).collect();
    for child in children {
        match locate_offset_in_event_space(chapter, child, offset) {
            Located::Found(n) => return Located::Found(n),
            Located::Remaining(r) => offset = r,
        }
    }
    Located::Remaining(offset)
}

/// One node's contribution to the offset walk. Counting mirrors the
/// calibre's `locate_offset_in`: text runs count code points,
/// replaced elements (images) count one position, ruby annotation text and
/// table/list containers are opaque, other containers descend in document
/// order.
fn locate_offset_in_ir(chapter: &mut Chapter, node: NodeId, mut offset: i64) -> Located {
    if offset < 0 {
        return Located::Remaining(offset);
    }
    let Some(n) = chapter.node(node) else {
        return Located::Remaining(offset);
    };
    let role = n.role;
    match role {
        Role::Text => {
            // Calibre splits a text run at `\n` into span text plus `<br>`
            // tail text, and its offset walk counts span text alone. Count the
            // chars before the first newline to match.
            let countable = chapter
                .text(n.text)
                .chars()
                .take_while(|&c| c != '\n')
                .count() as i64;
            if countable > 0 {
                if offset == 0 {
                    // The run starts exactly at the offset: calibre stamps the
                    // id on the run's own `<span>`. Wrap the run in a span
                    // carrying the id, never a zero-length span before it.
                    return Located::Found(wrap_text_run_in_span(chapter, node));
                }
                if offset < countable {
                    return Located::Found(split_text_run(chapter, node, offset));
                }
            }
            Located::Remaining(offset - countable)
        }
        // Replaced elements count as a single position; the id lands on the
        // element itself.
        Role::Image => {
            if offset == 0 {
                return Located::Found(node);
            }
            Located::Remaining(offset - 1)
        }
        // `<br>` contributes nothing (see the text-run rule above).
        Role::Break => Located::Remaining(offset),
        // A styled/event span whose own text starts exactly at the offset
        // carries the id itself, with no wrapping or nesting. A demoted
        // `render: inline` block holds no own text and descends as a container.
        Role::Inline
            if offset == 0
                && !chapter.semantics.render_inline(node)
                && chapter.children(node).next().is_some_and(|first| {
                    chapter.node(first).is_some_and(|f| {
                        f.role == Role::Text
                            && chapter
                                .text(f.text)
                                .chars()
                                .take_while(|&c| c != '\n')
                                .next()
                                .is_some()
                    })
                }) =>
        {
            Located::Found(node)
        }
        // Opaque subtrees. Ruby contributes nothing: calibre's offset walk
        // counts `<span>` direct text, skipping rb and never descending rt.
        // Table and list containers are never descended either.
        Role::Ruby
        | Role::RubyText
        | Role::Table
        | Role::TableHead
        | Role::TableBody
        | Role::OrderedList
        | Role::UnorderedList => Located::Remaining(offset),
        // Everything else descends in document order.
        _ => {
            let children: Vec<NodeId> = chapter.children(node).collect();
            for child in children {
                match locate_offset_in_ir(chapter, child, offset) {
                    Located::Found(n) => return Located::Found(n),
                    Located::Remaining(r) => offset = r,
                }
            }
            Located::Remaining(offset)
        }
    }
}

/// Wrap the text run `node` in a fresh anchor span: the span takes `node`'s
/// place in the sibling chain and `node` becomes its only child. The stamped
/// id then wraps the run's full text, matching calibre's
/// run-boundary stamp (`locate_offset` returning the existing `<span>`).
fn wrap_text_run_in_span(chapter: &mut Chapter, node: NodeId) -> NodeId {
    let parent = chapter.node(node).and_then(|n| n.parent);
    let old_next = chapter.node(node).and_then(|n| n.next_sibling);
    let span = chapter.alloc_node(Node::new(Role::Inline));
    if let Some(s) = chapter.node_mut(span) {
        s.parent = parent;
        s.next_sibling = old_next;
        s.first_child = Some(node);
    }
    if let Some(parent) = parent {
        // Relink: either the parent's first_child or the previous sibling
        // points at `node`; both are repointed at the span.
        let first = chapter.node(parent).and_then(|p| p.first_child);
        if first == Some(node) {
            if let Some(p) = chapter.node_mut(parent) {
                p.first_child = Some(span);
            }
        } else {
            let mut cur = first;
            while let Some(c) = cur {
                let next = chapter.node(c).and_then(|n| n.next_sibling);
                if next == Some(node) {
                    if let Some(prev) = chapter.node_mut(c) {
                        prev.next_sibling = Some(span);
                    }
                    break;
                }
                cur = next;
            }
        }
    }
    if let Some(n) = chapter.node_mut(node) {
        n.parent = Some(span);
        n.next_sibling = None;
    }
    span
}

/// Split the text run `node` at `char_offset` (0 < offset < len) code points:
/// the run keeps the head, a zero-length anchor span and the tail run follow
/// as new siblings. Returns the anchor span.
fn split_text_run(chapter: &mut Chapter, node: NodeId, char_offset: i64) -> NodeId {
    let (range, byte_split) = {
        let n = chapter.node(node).expect("split target exists");
        let text = chapter.text(n.text);
        let byte = text
            .char_indices()
            .nth(char_offset as usize)
            .map(|(b, _)| b)
            .unwrap_or(text.len());
        (n.text, byte as u32)
    };
    let old_next = chapter.node(node).and_then(|n| n.next_sibling);
    let parent = chapter.node(node).and_then(|n| n.parent);

    let span = chapter.alloc_node(Node::new(Role::Inline));
    let tail = chapter.alloc_node(Node::text(crate::model::TextRange::new(
        range.start + byte_split,
        range.len - byte_split,
    )));

    if let Some(n) = chapter.node_mut(node) {
        n.text = crate::model::TextRange::new(range.start, byte_split);
        n.next_sibling = Some(span);
    }
    if let Some(s) = chapter.node_mut(span) {
        s.parent = parent;
        s.next_sibling = Some(tail);
    }
    if let Some(t) = chapter.node_mut(tail) {
        t.parent = parent;
        t.next_sibling = old_next;
    }
    span
}

/// Emit the fields an element states about its place in a structure: a cell's
/// `$148 table_column_span` / `$149 table_row_span`, a table's `$152
/// column_format` list, and a list's `$104 list_start_offset`.
///
/// KFX gives a cell no element type of its own, and the span sits on whatever
/// element occupies the row's content list. A span of 1 is the default and is
/// left unwritten.
fn push_structural_fields(fields: &mut Vec<(u64, IonValue)>, elem: &ElementStart) {
    if let Some(n) = elem.column_span.filter(|n| *n > 1) {
        fields.push((sym!(TableColumnSpan), IonValue::Int(n as i64)));
    }
    if let Some(n) = elem.row_span.filter(|n| *n > 1) {
        fields.push((sym!(TableRowSpan), IonValue::Int(n as i64)));
    }
    if let Some(n) = elem.list_start {
        fields.push((sym!(ListStartOffset), IonValue::Int(n as i64)));
    }
    if !elem.column_format.is_empty() {
        let entries = elem
            .column_format
            .iter()
            .map(|entry| {
                let mut entry_fields: Vec<(u64, IonValue)> = entry
                    .fields
                    .iter()
                    .map(|(sym, value)| (*sym, value.to_ion()))
                    .collect();
                if let Some(n) = entry.span.filter(|n| *n > 1) {
                    entry_fields.push((sym!(ColumnSpan), IonValue::Int(n as i64)));
                }
                // A column that states nothing holds its place, and
                // Amazon spells that placeholder `{is_empty: false}`.
                if entry_fields.is_empty() {
                    entry_fields.push((sym!(IsEmpty), IonValue::Bool(false)));
                }
                IonValue::Struct(entry_fields)
            })
            .collect();
        fields.push((sym!(ColumnFormat), IonValue::List(entries)));
    }
}

/// Apply semantic attributes to a node from a generic map.
///
/// This is the **only place** that knows about SemanticTarget → IR mapping.
/// It's a simple dispatcher, not format-specific logic.
fn apply_semantics_to_node(
    chapter: &mut Chapter,
    node_id: NodeId,
    semantics: &HashMap<SemanticTarget, String>,
) {
    for (target, value) in semantics {
        match target {
            SemanticTarget::Src => chapter.semantics.set_src(node_id, value),
            SemanticTarget::Href => {
                chapter.semantics.set_href(node_id, value);
            }
            SemanticTarget::Alt => chapter.semantics.set_alt(node_id, value),
            SemanticTarget::Id => chapter.semantics.set_id(node_id, value),
            SemanticTarget::EpubType => chapter.semantics.set_epub_type(node_id, value),
        }
    }
}

/// Convert KFX style properties to an IR ComputedStyle using the schema.
///
/// This is schema-driven: iterates schema rules with KFX symbol mappings,
/// applies inverse transforms to convert KFX values back to IR values.
fn kfx_style_to_ir(
    props: &[(u64, IonValue)],
    symbols: &SymbolTable,
) -> crate::style::ComputedStyle {
    use crate::formats::kfx::style_schema::{StyleSchema, import_kfx_style};

    let schema = StyleSchema::standard();
    let mut style = import_kfx_style(schema, props);
    // `background_image` names an `external_resource` by symbol, past the
    // reach of the schema's value transforms. The IR keeps the resource name
    // until the importer swaps in the exported filename.
    if let Some(name) = get_field(props, sym!(BackgroundImage))
        .and_then(|v| v.as_symbol())
        .and_then(|id| symbols.resolve_opt(id))
    {
        style.background_image = Some(name.to_string());
    }
    style
}

/// Build text nodes with inline spans applied.
///
/// The `symbols` and `styles` parameters resolve span styles:
/// - `symbols`: resolves style symbol IDs to style names
/// - `styles`: maps style names to KFX style properties
fn build_text_with_spans(
    chapter: &mut Chapter,
    parent: NodeId,
    text: &str,
    spans: &[SpanStart],
    symbols: &SymbolTable,
    styles: Option<&HashMap<String, Vec<(u64, IonValue)>>>,
) {
    // KFX expresses a link and a ruby/styled run over one text as independent,
    // partly overlapping events. Links partition the outside and never nest; a
    // mark survives inside one link's range or gap, and is dropped across both.
    let total_chars = text.chars().count();
    // The links calibre's walk keeps: offset order, event order on ties, each
    // skipped when it starts inside the previous kept link's range.
    // Out-of-bounds events are dropped, never clamped.
    let mut kept_links: Vec<usize> = Vec::new();
    {
        let mut idx: Vec<usize> = (0..spans.len())
            .filter(|&i| {
                spans[i].role == Role::Link && spans[i].offset + spans[i].length <= total_chars
            })
            .collect();
        idx.sort_by_key(|&i| spans[i].offset);
        let mut cursor = 0usize;
        for i in idx {
            if spans[i].offset < cursor {
                continue;
            }
            cursor = spans[i].offset + spans[i].length;
            kept_links.push(i);
        }
    }
    let link_bounds: Vec<(usize, usize)> = kept_links
        .iter()
        .map(|&i| (spans[i].offset, spans[i].offset + spans[i].length))
        .collect();
    let kept_links: std::collections::HashSet<usize> = kept_links.into_iter().collect();
    let fits_segment = |off: usize, end: usize| -> bool {
        // Inside one link's range?
        if link_bounds.iter().any(|&(s, e)| off >= s && end <= e) {
            return true;
        }
        // Inside one inter-link gap?
        let mut gap_start = 0usize;
        for &(s, e) in &link_bounds {
            if off >= gap_start && end <= s {
                return true;
            }
            gap_start = e;
        }
        off >= gap_start && end <= total_chars
    };
    let spans: Vec<&SpanStart> = spans
        .iter()
        .enumerate()
        .filter(|(i, s)| {
            if s.role == Role::Link {
                kept_links.contains(i)
            } else if link_bounds.is_empty() {
                true
            } else {
                s.offset + s.length <= total_chars && fits_segment(s.offset, s.offset + s.length)
            }
        })
        .map(|(_, s)| s)
        .collect();

    // A ruby's interior is opaque: its `<rb>` slices come straight from the
    // text and a mark starting inside its range is skipped, while a mark
    // containing the ruby nests it. `ruby_bounds` drops the interior marks.
    let ruby_bounds: Vec<(usize, usize)> = spans
        .iter()
        .filter(|s| s.role == Role::Ruby)
        .map(|s| (s.offset, s.length))
        .collect();
    let spans: Vec<&SpanStart> = spans
        .into_iter()
        .filter(|s| {
            !ruby_bounds.iter().any(|&(ro, rl)| {
                let interior = s.offset >= ro && s.offset < ro + rl;
                let is_self = s.role == Role::Ruby && s.offset == ro && s.length == rl;
                let contains_ruby = s.offset == ro && s.length > rl;
                interior && !is_self && !contains_ruby
            })
        })
        .collect();

    // Build a nested span tree: sort by offset, then length DESCENDING
    // (enclosing spans first), which nests containment below; a link ties ahead
    // of an equal-range mark (the mark renders inside the `<a>`).
    let mut sorted_spans: Vec<_> = spans;
    sorted_spans.sort_by(|a, b| {
        a.offset
            .cmp(&b.offset)
            .then_with(|| b.length.cmp(&a.length)) // Larger spans first at same offset
            .then_with(|| (b.role == Role::Link).cmp(&(a.role == Role::Link)))
    });

    // Helper to create a span node with style and semantics applied
    let create_span_node = |chapter: &mut Chapter, span: &SpanStart| -> NodeId {
        let span_node = chapter.alloc_node(Node::new(span.role));

        // Apply style from the styles map (if present); the resolved KFX
        // style name doubles as the span's source class identity.
        if let Some(style_sym) = span.style_symbol
            && let Some(style_name) = symbols.resolve_opt(style_sym)
        {
            chapter.semantics.set_class(span_node, style_name);
            if let Some(styles_map) = styles
                && let Some(kfx_props) = styles_map.get(style_name)
            {
                let ir_style = kfx_style_to_ir(kfx_props, symbols);
                let style_id = chapter.styles.intern(ir_style);
                if let Some(node) = chapter.node_mut(span_node) {
                    node.style = style_id;
                }
            }
        }

        // Apply ALL semantic attributes from the generic map
        apply_semantics_to_node(chapter, span_node, &span.semantics);

        span_node
    };

    // Stack of (node_id, char_end_offset, ruby_annotation_to_attach_on_close)
    // for active spans. A Ruby span carries the annotation text resolved from
    // ruby_content, attached as an `<rt>` child when the span closes.
    let mut span_stack: Vec<(NodeId, usize, Option<String>)> = vec![(parent, usize::MAX, None)];
    let mut char_pos: usize = 0; // Current position in char offsets

    // Closure to append a `<rt>` child holding the annotation. Called when a
    // Ruby span is popped from the stack.
    fn append_ruby_text(chapter: &mut Chapter, ruby_node: NodeId, annotation: &str) {
        let range = chapter.append_text(annotation);
        let text_node = chapter.alloc_node(Node::text(range));
        let rt_node = chapter.alloc_node(Node::new(Role::RubyText));
        chapter.append_child(rt_node, text_node);
        chapter.append_child(ruby_node, rt_node);
    }

    for span in sorted_spans {
        let span_start = span.offset;
        let span_end = span.offset + span.length;

        // Pop any spans that have ended before this span starts
        while span_stack.len() > 1 {
            let (_, stack_end, _) = span_stack.last().unwrap();
            if *stack_end <= span_start {
                // This span has ended - add any remaining text to it first
                if char_pos < *stack_end {
                    let byte_start = char_to_byte_offset(text, char_pos);
                    let byte_end = char_to_byte_offset(text, *stack_end);
                    if byte_end > byte_start {
                        let segment = &text[byte_start..byte_end];
                        let range = chapter.append_text(segment);
                        let text_node = chapter.alloc_node(Node::text(range));
                        let (parent_id, _, _) = span_stack.last().unwrap();
                        chapter.append_child(*parent_id, text_node);
                    }
                    char_pos = *stack_end;
                }
                let (closing_node, _, ruby_ann) = span_stack.pop().unwrap();
                if let Some(annotation) = ruby_ann {
                    append_ruby_text(chapter, closing_node, &annotation);
                }
            } else {
                break;
            }
        }

        // A span starting inside a consumed range (only a grouped
        // ruby consumes ahead) is malformed nesting, skipped here.
        if span_start < char_pos {
            continue;
        }

        // Add text between char_pos and span_start to current parent
        if char_pos < span_start {
            let byte_start = char_to_byte_offset(text, char_pos);
            let byte_end = char_to_byte_offset(text, span_start);
            if byte_end > byte_start {
                let before = &text[byte_start..byte_end];
                let range = chapter.append_text(before);
                let text_node = chapter.alloc_node(Node::text(range));
                let (current_parent, _, _) = span_stack.last().unwrap();
                chapter.append_child(*current_parent, text_node);
            }
            char_pos = span_start;
        }

        // Grouped ruby (`ruby_id_list`): one `<ruby>` holding interleaved
        // base-slice / `<rt>` pairs, built whole with nothing nested inside,
        // and the cursor jumps past the event's range.
        if span.role == Role::Ruby && !span.ruby_pairs.is_empty() && span_end > span_start {
            let ruby_node = create_span_node(chapter, span);
            let (current_parent, _, _) = span_stack.last().unwrap();
            chapter.append_child(*current_parent, ruby_node);
            let total_chars = text.chars().count();
            for (sub_off, sub_len, annotation) in &span.ruby_pairs {
                let s = span_start + sub_off;
                let e = s + sub_len;
                if e > total_chars {
                    break;
                }
                let byte_s = char_to_byte_offset(text, s);
                let byte_e = char_to_byte_offset(text, e);
                if byte_e > byte_s {
                    let range = chapter.append_text(&text[byte_s..byte_e]);
                    let text_node = chapter.alloc_node(Node::text(range));
                    chapter.append_child(ruby_node, text_node);
                }
                append_ruby_text(chapter, ruby_node, annotation);
            }
            char_pos = span_end;
            continue;
        }

        // Create this span and push onto stack
        if span_end > span_start {
            let span_node = create_span_node(chapter, span);
            let (current_parent, _, _) = span_stack.last().unwrap();
            chapter.append_child(*current_parent, span_node);
            span_stack.push((span_node, span_end, span.ruby_annotation.clone()));
        }
    }

    // Close all remaining spans and add trailing text
    while let Some((node_id, end_offset, ruby_ann)) = span_stack.pop() {
        let actual_end = end_offset.min(text.chars().count());
        if char_pos < actual_end {
            let byte_start = char_to_byte_offset(text, char_pos);
            let byte_end = char_to_byte_offset(text, actual_end);
            if byte_end > byte_start {
                let segment = &text[byte_start..byte_end];
                let range = chapter.append_text(segment);
                let text_node = chapter.alloc_node(Node::text(range));
                chapter.append_child(node_id, text_node);
            }
            char_pos = actual_end;
        }
        if let Some(annotation) = ruby_ann {
            append_ruby_text(chapter, node_id, &annotation);
        }
    }
}

/// Convert a character (code point) offset to a byte offset.
///
/// KFX style_events use character offsets, not byte offsets. This function
/// converts the char offset to a byte offset for string slicing.
fn char_to_byte_offset(text: &str, char_offset: usize) -> usize {
    text.char_indices()
        .nth(char_offset)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(text.len())
}

// ============================================================================
// Helper functions
// ============================================================================

/// Resolve a value that could be either a symbol or string.
fn resolve_symbol_or_string(value: &IonValue, symbols: &SymbolTable) -> Option<String> {
    symbols.text_of_opt(value).map(str::to_string)
}

// ============================================================================
// High-level API (used by KfxImporter)
// ============================================================================

/// Parse a storyline and build IR in one step.
///
/// This is the main entry point for KFX import.
///
/// The `anchors` map resolves external links (anchor_name → uri).
/// The `styles` map resolves style references (style_name → properties).
/// The `anchor_table` stamps html ids at anchored `(eid, offset)` positions.
pub fn parse_storyline_to_ir<F>(
    storyline: &IonValue,
    symbols: &SymbolTable,
    anchors: Option<&HashMap<String, String>>,
    styles: Option<&HashMap<String, Vec<(u64, IonValue)>>>,
    ruby_index: Option<&HashMap<String, Vec<String>>>,
    anchor_table: Option<&AnchorTable>,
    content_lookup: F,
) -> Chapter
where
    F: FnMut(&str, usize) -> Option<String>,
{
    let tokens = tokenize_storyline(storyline, symbols, anchors, styles, ruby_index);
    build_ir_from_tokens_anchored(&tokens, symbols, styles, anchor_table, content_lookup)
}

// ============================================================================
// EXPORT: IR → TokenStream → Ion
// ============================================================================

use crate::formats::kfx::context::ExportContext;
use crate::style::{ComputedStyle, Length};

/// Check if a style has borders that require container wrapping in KFX.
///
/// KFX requires block elements with borders to be wrapped in a `type: container`
/// with nested `type: text` for the content. Without this wrapper, borders don't
/// render on Kindle devices.
fn needs_container_wrapper(style: &ComputedStyle) -> bool {
    let has_top = style.border_style_top.draws()
        && !matches!(style.border_width_top, Length::Auto | Length::Px(0.0));
    let has_bottom = style.border_style_bottom.draws()
        && !matches!(style.border_width_bottom, Length::Auto | Length::Px(0.0));
    let has_left = style.border_style_left.draws()
        && !matches!(style.border_width_left, Length::Auto | Length::Px(0.0));
    let has_right = style.border_style_right.draws()
        && !matches!(style.border_width_right, Length::Auto | Length::Px(0.0));
    has_top || has_bottom || has_left || has_right
}

/// Whether a run of text is a tate-chu-yoko candidate: a 1–2 character run of
/// ASCII digits (chapter numbers, short counts). Longer runs (years like
/// `1999`) are left to rotate, matching conventional vertical CJK typography
/// where only short numbers are combined upright.
fn is_short_ascii_digit_run(text: &str) -> bool {
    matches!(text.len(), 1 | 2) && text.bytes().all(|b| b.is_ascii_digit())
}

/// Roles that flatten into their parent's inline text run (Text/Break become
/// characters; Link/Inline/Ruby become style_events) with no KFX structure
/// of their own. Distinguishes a bordered *leaf* element (inline
/// content only → inner-text wrapper) from a bordered element with *block*
/// children (e.g. a `罫囲み` `<div>` of `<p>` lines → one `type: container`).
fn is_inline_like_role(role: Role) -> bool {
    matches!(
        role,
        Role::Text | Role::Inline | Role::Link | Role::Ruby | Role::RubyText | Role::Break
    )
}

/// Convert an IR chapter to a TokenStream.
///
/// This is the first stage of export: walking the IR tree and emitting tokens.
pub fn ir_to_tokens(chapter: &Chapter, ctx: &mut ExportContext) -> TokenStream {
    let sch = schema();
    let mut stream = TokenStream::new();

    walk_node_for_export(chapter, chapter.root(), sch, ctx, &mut stream);
    stream
}

/// Walk a node and emit tokens for export.
///
/// Attributes are exported through the schema, never hardcoded. Inline roles
/// (Link, Inline) are emitted as StartSpan/EndSpan, not
/// StartElement/EndElement, which is what lets style_events be generated.
fn walk_node_for_export(
    chapter: &Chapter,
    node_id: NodeId,
    sch: &crate::formats::kfx::schema::KfxSchema,
    ctx: &mut ExportContext,
    stream: &mut TokenStream,
) {
    let node = match chapter.node(node_id) {
        Some(n) => n,
        None => return,
    };

    // Root node: walk children, wrapping a loose inline-ish child in a
    // synthetic Paragraph. Loose inline content under <body> emits
    // Text/StartSpan/EndSpan onto the root IonBuilder, which drops it.
    if node.role == Role::Root {
        for child_id in chapter.children(node_id) {
            let Some(child) = chapter.node(child_id) else {
                continue;
            };
            if matches!(
                child.role,
                Role::Text | Role::Break | Role::Inline | Role::Link | Role::Ruby
            ) {
                // Wrap in a synthetic Paragraph, a real element for the inline
                // emit machinery to anchor style_events to.
                let mut wrapper = ElementStart::new(Role::Paragraph);
                wrapper.style_symbol = Some(ctx.default_style_symbol);
                stream.push(KfxToken::StartElement(wrapper));
                walk_node_for_export(chapter, child_id, sch, ctx, stream);
                stream.push(KfxToken::EndElement);
            } else {
                walk_node_for_export(chapter, child_id, sch, ctx, stream);
            }
        }
        return;
    }

    // Text nodes: emit just the text, not a container
    // Text nodes are leaf nodes that contain the actual string data
    if node.role == Role::Text {
        if !node.text.is_empty() {
            let text = chapter.text(node.text);
            if !text.is_empty() {
                if ctx.is_vertical_document() && is_short_ascii_digit_run(text) {
                    // Tate-chu-yoko (縦中横): a standalone short digit run goes
                    // upright in one cell, in the inline `text_combine` span
                    // `register_tatechuyoko_style` registers.
                    let combine = ctx.register_tatechuyoko_style();
                    let mut span = SpanStart::new(Role::Inline, 0, 0);
                    span.style_symbol = Some(combine);
                    stream.push(KfxToken::StartSpan(span));
                    stream.push(KfxToken::Text(text.to_string()));
                    stream.push(KfxToken::EndSpan);
                } else {
                    stream.push(KfxToken::Text(text.to_string()));
                }
            }
        }
        return;
    }

    // Break nodes: emit a newline character
    // KFX represents <br> as \n within text content, not as separate elements
    if node.role == Role::Break {
        stream.push(KfxToken::Text("\n".to_string()));
        return;
    }

    // Definition lists: group dt+dd pairs into wrapper elements
    // HTML has dt/dd as flat siblings, but KFX needs them grouped for float to work
    if node.role == Role::DefinitionList {
        emit_definition_list(chapter, node_id, sch, ctx, stream);
        return;
    }

    // Inline elements (Link, Inline, Ruby): use the flattening algorithm.
    // This produces non-overlapping style_events where each text segment
    // carries the accumulated state from all ancestors.
    if node.role == Role::Link || node.role == Role::Inline || node.role == Role::Ruby {
        emit_inline_content_flat(chapter, node_id, sch, ctx, stream);
        return;
    }

    // RubyText is consumed by its parent Ruby during inline flattening. An
    // `<rt>` outside a `<ruby>` is dropped, and its annotation characters
    // reach no base text stream.
    if node.role == Role::RubyText {
        return;
    }

    // Get KFX type from schema (will be used in tokens_to_ion)
    let _kfx_type = sch.kfx_type_for_role(node.role);

    // Build element start with semantics
    let mut elem = ElementStart::new(node.role);
    elem.node_id = Some(node_id);

    // Register the node's style: IR ComputedStyle → a deduplicated KFX style
    // symbol. The source class attribute is a name hint, carrying an
    // identifier like "bold" / "vrtl" into the symbol table over an `s<N>`.
    let class_hint = chapter.semantics.class(node_id);
    // A cell's span joins its style, not its element — that is where
    // Amazon writes it, and it is the only place the corpus attests.
    let spans = cell_span_properties(chapter, node_id);
    let style_symbol =
        ctx.register_style_id_with_extras(node.style, &chapter.styles, class_hint, &spans);
    elem.style_symbol = Some(style_symbol);

    // A border renders on a `type: container` holding a nested `type: text`.
    // A horizontal rule is exempt: its border *is* the rule, drawn from the
    // bare `{style: linear, type: horizontal_rule}` element Amazon emits.
    elem.needs_container_wrapper = node.role != Role::Rule
        && chapter
            .styles
            .get(node.style)
            .map(needs_container_wrapper)
            .unwrap_or(false);

    // Does this element have block-level children (vs. only inline/text)? A
    // bordered `<div>` of `<p>` lines (Aozora 罫囲み) becomes one bordered
    // `type: container`; a bordered leaf `<p>` keeps the inner-text wrapper.
    elem.has_block_children = chapter.children(node_id).any(|c| {
        chapter
            .node(c)
            .is_some_and(|n| !is_inline_like_role(n.role))
    });

    // A `type: container`'s `layout` is its children's block-progression axis,
    // keyed to the box's own resolved writing mode: `horizontal` for vertical
    // text (縦書き), `vertical` for horizontal-tb.
    if elem.needs_container_wrapper {
        use crate::style::WritingMode;
        let wm = chapter
            .styles
            .get(node.style)
            .map(|s| s.writing_mode)
            .unwrap_or_default();
        let layout = match wm {
            WritingMode::VerticalRl | WritingMode::VerticalLr => KfxSymbol::Horizontal,
            WritingMode::HorizontalTb => KfxSymbol::Vertical,
        };
        elem.container_layout = Some(layout as u64);
    }

    // SCHEMA-DRIVEN attribute export
    // Create a closure to get semantic values by target
    let export_ctx = crate::formats::kfx::transforms::ExportContext {
        spine_map: None,
        resource_registry: Some(&ctx.resource_registry),
    };
    let mut kfx_attrs = sch.export_attributes(
        node.role,
        |target| match target {
            SemanticTarget::Href => chapter.semantics.href(node_id).map(|s| s.to_string()),
            SemanticTarget::Src => chapter.semantics.src(node_id).map(|s| s.to_string()),
            SemanticTarget::Alt => chapter.semantics.alt(node_id).map(|s| s.to_string()),
            SemanticTarget::Id => chapter.semantics.id(node_id).map(|s| s.to_string()),
            SemanticTarget::EpubType => chapter.semantics.epub_type(node_id).map(|s| s.to_string()),
        },
        &export_ctx,
    );

    // A `link_to` carries the anchor symbol its href reaches; an href that
    // reaches nothing carries no link.
    kfx_attrs.retain_mut(|(field_id, value)| {
        if *field_id != sym!(LinkTo) {
            return true;
        }
        match ctx.anchor_registry.link_symbol(value) {
            Some(symbol) => {
                *value = symbol;
                true
            }
            None => false,
        }
    });

    // Store the transformed KFX attributes for tokens_to_ion
    elem.kfx_attrs = kfx_attrs;

    // The element's own semantic map, beside the KFX attributes above.
    if let Some(href) = chapter.semantics.href(node_id) {
        elem.set_semantic(SemanticTarget::Href, href.to_string());
    }
    if let Some(src) = chapter.semantics.src(node_id) {
        elem.set_semantic(SemanticTarget::Src, src.to_string());
        // Intern any referenced resources.
        ctx.resource_registry.register(src, &mut ctx.symbols);
    }
    if let Some(alt) = chapter.semantics.alt(node_id) {
        elem.set_semantic(SemanticTarget::Alt, alt.to_string());
    }
    if let Some(id) = chapter.semantics.id(node_id) {
        elem.set_semantic(SemanticTarget::Id, id.to_string());
    }
    if let Some(epub_type) = chapter.semantics.epub_type(node_id) {
        elem.set_semantic(SemanticTarget::EpubType, epub_type.to_string());
    }

    // A table states its column geometry on itself, and its `<colgroup>`
    // collapses into a `column_format` field, with no element of its own.
    if node.role == Role::Table {
        elem.column_format = collect_column_format(chapter, node_id, ctx);
    }

    elem.list_start = chapter.semantics.list_start(node_id);

    stream.push(KfxToken::StartElement(elem));

    // Emit text content if present
    if !node.text.is_empty() {
        let text = chapter.text(node.text);
        if !text.is_empty() {
            stream.push(KfxToken::Text(text.to_string()));
        }
    }

    // Walk children. A column group is consumed by its table's
    // `column_format`; walking it too puts stray empty elements in the
    // content list, where a renderer reads them as rows.
    for child in chapter.children(node_id) {
        if chapter
            .node(child)
            .is_some_and(|n| n.role == Role::ColumnGroup)
        {
            continue;
        }
        walk_node_for_export(chapter, child, sch, ctx, stream);
    }

    stream.push(KfxToken::EndElement);
}

/// Read `$148 table_column_span` / `$149 table_row_span` out of a named
/// style's properties. A span of one is the absent case.
fn cell_spans_from_style(props: &[(u64, IonValue)]) -> (Option<u32>, Option<u32>) {
    let span_of = |field: u64| {
        get_field(props, field)
            .and_then(|v| v.as_int())
            .filter(|n| *n > 1)
            .map(|n| n as u32)
    };
    (span_of(sym!(TableColumnSpan)), span_of(sym!(TableRowSpan)))
}

/// A cell's span as KFX style properties, empty for a node that spans one.
fn cell_span_properties(
    chapter: &Chapter,
    node_id: NodeId,
) -> Vec<(KfxSymbol, crate::formats::kfx::style_schema::KfxValue)> {
    use crate::formats::kfx::style_schema::KfxValue;
    let mut out = Vec::new();
    if let Some(n) = chapter.semantics.col_span(node_id).filter(|n| *n > 1) {
        out.push((KfxSymbol::TableColumnSpan, KfxValue::Integer(n as i64)));
    }
    if let Some(n) = chapter.semantics.row_span(node_id).filter(|n| *n > 1) {
        out.push((KfxSymbol::TableRowSpan, KfxValue::Integer(n as i64)));
    }
    out
}

/// Find a table's column group. HTML puts it before the row sections, but a
/// parser that infers a `<tbody>` can nest it one level down, and the walk
/// looks through the section wrappers too.
fn find_column_group(chapter: &Chapter, table: NodeId) -> Option<NodeId> {
    let is_group = |id: NodeId| {
        chapter
            .node(id)
            .is_some_and(|n| n.role == Role::ColumnGroup)
            .then_some(id)
    };
    chapter.children(table).find_map(|child| {
        is_group(child).or_else(|| {
            matches!(
                chapter.node(child).map(|n| n.role),
                Some(Role::TableHead | Role::TableBody)
            )
            .then(|| chapter.children(child).find_map(is_group))
            .flatten()
        })
    })
}

/// Collect a table's `<col>` entries into KFX `column_format` entries.
///
/// A column's width is ordinary styling, arriving as the node's computed
/// style and converts through the same export schema every other style uses;
/// only the geometry properties belong in the entry, since a `<col>` inherits
/// the rest from the table.
fn collect_column_format(
    chapter: &Chapter,
    table: NodeId,
    ctx: &ExportContext,
) -> Vec<ColumnFormat> {
    const GEOMETRY: &[KfxSymbol] = &[KfxSymbol::Width, KfxSymbol::SizingBounds];
    let Some(group) = find_column_group(chapter, table) else {
        return Vec::new();
    };
    // One entry per `<col>`, placeholders included — the list is read
    // positionally at the other end too.
    chapter
        .children(group)
        .map(|col| {
            let fields = chapter
                .node(col)
                .and_then(|n| chapter.styles.get(n.style))
                .map(|ir_style| {
                    ctx.kfx_properties(ir_style)
                        .into_iter()
                        .filter(|(sym, _)| GEOMETRY.contains(sym))
                        .map(|(sym, value)| (sym as u64, value))
                        .collect()
                })
                .unwrap_or_default();
            ColumnFormat {
                fields,
                span: chapter.semantics.col_span(col),
            }
        })
        .collect()
}

// ============================================================================
// Inline content flattening
// ============================================================================

// Nested inline elements (Link, Inline, Text) become flat KFX style_events,
// each over a disjoint text range, carrying every attribute its ancestors
// declare. A depth-first walk emits one event per text leaf.

/// Active state during inline flattening - accumulated from ancestors.
#[derive(Clone, Default)]
struct InlineState {
    /// Active link target (from Link ancestor), as anchor symbol string
    link_to: Option<String>,
    /// Active style (innermost wins)
    style: Option<crate::style::StyleId>,
    /// Active epub:type for noteref detection
    epub_type: Option<String>,
    /// Active element ID (for anchor creation)
    element_id: Option<String>,
    /// Active node ID (for anchor creation with GlobalNodeId)
    node_id: Option<NodeId>,
    /// Source `class` attribute of the inline node owning the active style.
    /// Tracked alongside `style` (innermost-wins) for the KFX style registry
    /// keeps names like `bold`, `tcy`, `upright` in place of
    /// synthesized `s<N>` symbols.
    class_hint: Option<String>,
    /// Ruby annotation text from a Ruby ancestor (the <rt> content).
    /// When set, the base text segments get a ruby_name + ruby_id style_event
    /// pointing at an entry in a ruby_content fragment.
    ruby_annotation: Option<String>,
}

/// A flattened segment with its computed state. Most segments carry text,
/// but an inline `<img>` (e.g. a gaiji glyph used as the base of a `<ruby>`)
/// emits as an Image variant, which carries the image element through:
/// `flatten_inline_content` emits text leaves alone, and an image that is no
/// direct child of a block-level element has no other way out.
enum FlatSegment {
    Text { text: String, state: InlineState },
    // An image segment carries no inline state: the surrounding ruby
    // annotation / link_to / inline style reaches no KFX image element. The
    // node alone is tracked, and `walk_node_for_export` refetches src/alt/style.
    Image { node_id: NodeId },
}

/// Recursively concatenate all Text descendants of a node into `out`.
///
/// Used by the Role::Ruby flatten arm to gather the <rt> annotation
/// content (which may itself contain inline wrappers).
fn collect_text_recursive(chapter: &Chapter, node_id: NodeId, out: &mut String) {
    let Some(node) = chapter.node(node_id) else {
        return;
    };
    if node.role == Role::Text && !node.text.is_empty() {
        out.push_str(chapter.text(node.text));
    }
    for cid in chapter.children(node_id) {
        collect_text_recursive(chapter, cid, out);
    }
}

/// Flatten inline content into segments with computed state.
///
/// This is the "Push Down, Emit at Bottom" algorithm:
/// - Traverse the tree depth-first
/// - Accumulate state (link_to, style) on the way down
/// - Emit segments at Text leaves
fn flatten_inline_content(
    chapter: &Chapter,
    node_id: NodeId,
    state: InlineState,
    segments: &mut Vec<FlatSegment>,
) {
    let node = match chapter.node(node_id) {
        Some(n) => n,
        None => return,
    };

    // MERGE STATE: Calculate effective state for this node
    // Track both element_id (string) and node_id (for GlobalNodeId lookup)
    let has_id = chapter.semantics.id(node_id).is_some();
    let effective_state = InlineState {
        // Links: propagate down (newest wins if nested)
        link_to: chapter
            .semantics
            .href(node_id)
            .map(|s| s.to_string())
            .or(state.link_to),
        // Styles: innermost wins (child overrides parent)
        style: if node.role == Role::Inline || node.role == Role::Link {
            Some(node.style)
        } else {
            state.style
        },
        // Class hint moves alongside style — when the innermost inline wins
        // on style, it also contributes its source class name.
        class_hint: if node.role == Role::Inline || node.role == Role::Link {
            chapter.semantics.class(node_id).map(|s| s.to_string())
        } else {
            state.class_hint.clone()
        },
        // epub:type: propagate for noteref detection
        epub_type: chapter
            .semantics
            .epub_type(node_id)
            .map(|s| s.to_string())
            .or(state.epub_type),
        // Element ID: for anchor creation (string ID)
        element_id: chapter
            .semantics
            .id(node_id)
            .map(|s| s.to_string())
            .or(state.element_id),
        // Node ID: track which node has the ID (for GlobalNodeId lookup)
        node_id: if has_id { Some(node_id) } else { state.node_id },
        // Ruby annotation: set explicitly by the Role::Ruby arm below, and
        // inherited by a nested Inline inside a Ruby down to its Text leaves.
        ruby_annotation: state.ruby_annotation.clone(),
    };

    match node.role {
        // TEXT LEAVES: Emit segment with accumulated state
        Role::Text => {
            if !node.text.is_empty() {
                let text = chapter.text(node.text);
                if !text.is_empty() {
                    segments.push(FlatSegment::Text {
                        text: text.to_string(),
                        state: effective_state,
                    });
                }
            }
        }
        // BREAK: Emit newline as text
        Role::Break => {
            segments.push(FlatSegment::Text {
                text: "\n".to_string(),
                state: effective_state,
            });
        }
        // A leaf image, emitted as an Image segment. The catch-all below
        // recurses into the empty children of an `<img>` inside
        // `<ruby>`/`<a>`/`<span>` and emits nothing.
        Role::Image => {
            // A KFX image element carries no anchor. An ancestor inline element
            // bearing an id (`<a id="map1"><img/></a>`) takes a zero-width-space
            // span first, holding the id's position just before the image.
            if effective_state.element_id.is_some() {
                segments.push(FlatSegment::Text {
                    text: "\u{200B}".to_string(), // Zero-width space
                    state: effective_state.clone(),
                });
            }
            segments.push(FlatSegment::Image { node_id });
        }
        // RUBY TEXT: never recursed into here. Consumed by the parent Ruby
        // arm below — its text becomes the annotation, not inline content.
        // If <rt> appears outside <ruby> (malformed input), drop it silently.
        Role::RubyText => {}
        // Ruby: pair base content with rt annotations, splitting children into
        // (base_run, rt_run) pairs. Each base segment takes the matching rt's
        // text as its ruby_annotation, a compound ruby included.
        Role::Ruby => {
            let children: Vec<NodeId> = chapter.children(node_id).collect();
            let mut i = 0;
            while i < children.len() {
                // Collect consecutive non-rt children as the base for the next pair.
                let mut base_children: Vec<NodeId> = Vec::new();
                while i < children.len() {
                    let cid = children[i];
                    let role = chapter.node(cid).map(|n| n.role).unwrap_or(Role::Text);
                    if role == Role::RubyText {
                        break;
                    }
                    base_children.push(cid);
                    i += 1;
                }
                // Collect consecutive rt children as the annotation. Usually one,
                // but EPUBs sometimes have multiple <rt>s in a row (e.g. two
                // separate annotation lines); concatenate.
                let mut annotation = String::new();
                while i < children.len() {
                    let cid = children[i];
                    let role = chapter.node(cid).map(|n| n.role).unwrap_or(Role::Text);
                    if role != Role::RubyText {
                        break;
                    }
                    collect_text_recursive(chapter, cid, &mut annotation);
                    i += 1;
                }
                // Recurse into base children with annotation set (if any).
                let mut pair_state = effective_state.clone();
                if !annotation.is_empty() {
                    pair_state.ruby_annotation = Some(annotation);
                }
                for cid in base_children {
                    flatten_inline_content(chapter, cid, pair_state.clone(), segments);
                }
            }
        }
        // CONTAINERS (Link, Inline, etc.): Recurse with accumulated state
        _ => {
            let children: Vec<_> = chapter.children(node_id).collect();
            if children.is_empty() && effective_state.element_id.is_some() {
                // Empty element with ID (anchor marker) - emit zero-width space to carry the ID
                segments.push(FlatSegment::Text {
                    text: "\u{200B}".to_string(), // Zero-width space
                    state: effective_state,
                });
            } else {
                for child_id in children {
                    flatten_inline_content(chapter, child_id, effective_state.clone(), segments);
                }
            }
        }
    }
}

/// Convert flattened segments into KfxTokens (Text + style info for style_events).
///
/// This emits the text and creates SpanStart markers that will become style_events.
fn emit_flattened_segments(
    segments: Vec<FlatSegment>,
    chapter: &Chapter,
    sch: &crate::formats::kfx::schema::KfxSchema,
    ctx: &mut ExportContext,
    stream: &mut TokenStream,
) {
    for segment in segments {
        match segment {
            FlatSegment::Image { node_id } => {
                // Inline image leaf (e.g. gaiji inside <ruby>). Re-entering
                // walk_node_for_export gives the image element the src/alt/
                // style/resource registration a block-level image takes.
                walk_node_for_export(chapter, node_id, sch, ctx, stream);
            }
            FlatSegment::Text { text, state } => {
                let needs_style_event = state.link_to.is_some()
                    || state.style.is_some()
                    || state.ruby_annotation.is_some();

                if needs_style_event {
                    // Build span with accumulated state
                    let mut span = SpanStart::new(
                        if state.link_to.is_some() {
                            Role::Link
                        } else {
                            Role::Inline
                        },
                        0,
                        0,
                    );

                    // The innermost style wins, on the source class name the
                    // segment state's hint carries. A segment inside a Link
                    // takes the link-aware registration and an `underline`.
                    if let Some(style_id) = state.style {
                        let style_symbol = if state.link_to.is_some() {
                            ctx.register_link_style_id_with_hint(
                                style_id,
                                &chapter.styles,
                                state.class_hint.as_deref(),
                            )
                        } else {
                            ctx.register_style_id_with_hint(
                                style_id,
                                &chapter.styles,
                                state.class_hint.as_deref(),
                            )
                        };
                        span.style_symbol = Some(style_symbol);
                    }

                    // Build KFX attributes
                    let mut kfx_attrs = Vec::new();

                    // Add link_to when the href reaches an anchor this book holds
                    if let Some(ref href) = state.link_to
                        && let Some(anchor_symbol) = ctx.anchor_registry.link_symbol(href)
                    {
                        kfx_attrs.push((sym!(LinkTo), anchor_symbol));
                    }

                    // Add yj.display for noterefs
                    if let Some(ref epub_type) = state.epub_type
                        && epub_type.split_whitespace().any(|t| t == "noteref")
                    {
                        // YjNote = 617
                        kfx_attrs.push((sym!(YjDisplay), "617".to_string()));
                    }

                    // Add ruby_name + ruby_id if this segment is base text under a <ruby>.
                    // ruby_name resolves to the kfx_id of the ruby_content fragment that
                    // holds the annotation; ruby_id is the 1-indexed entry within it.
                    if let Some(ref annotation) = state.ruby_annotation {
                        let (frag_idx, ruby_id) = ctx.ruby_registry.register(annotation);
                        let ruby_name = format!("b_ruby_{}", frag_idx);
                        kfx_attrs.push((sym!(RubyName), ruby_name));
                        kfx_attrs.push((sym!(RubyId), ruby_id.to_string()));
                    }

                    span.kfx_attrs = kfx_attrs;

                    // Store element ID and node_id for anchor creation
                    if let Some(ref id) = state.element_id {
                        span.set_semantic(SemanticTarget::Id, id.clone());
                    }
                    span.node_id = state.node_id;

                    stream.push(KfxToken::StartSpan(span));
                    stream.push(KfxToken::Text(text));
                    stream.push(KfxToken::EndSpan);
                } else {
                    // Plain text, no style event needed
                    stream.push(KfxToken::Text(text));
                }
            }
        }
    }
}

/// Emit a definition list with dt+dd pairs grouped together.
///
/// HTML `<dl>` has `<dt>` and `<dd>` as flat siblings, but KFX needs each
/// dt+dd pair wrapped in a container for float:left to work properly.
/// This matches KPR's output structure:
///   Paragraph (wrapper)
///     Container (dt with float:left)
///       Paragraph (dt content)
///         Link
///     Paragraph (dd content)
///       Link
fn emit_definition_list(
    chapter: &Chapter,
    node_id: NodeId,
    sch: &crate::formats::kfx::schema::KfxSchema,
    ctx: &mut ExportContext,
    stream: &mut TokenStream,
) {
    let node = match chapter.node(node_id) {
        Some(n) => n,
        None => return,
    };

    // Emit the outer dl container (becomes Paragraph like KPR)
    let mut dl_elem = ElementStart::new(Role::Paragraph);
    let dl_style = ctx.register_style_id_with_hint(
        node.style,
        &chapter.styles,
        chapter.semantics.class(node_id),
    );
    dl_elem.style_symbol = Some(dl_style);

    stream.push(KfxToken::StartElement(dl_elem));

    // Collect children and group dt+dd pairs
    let children: Vec<NodeId> = chapter.children(node_id).collect();
    let mut i = 0;

    while i < children.len() {
        let child_id = children[i];
        let child = match chapter.node(child_id) {
            Some(n) => n,
            None => {
                i += 1;
                continue;
            }
        };

        if child.role == Role::DefinitionTerm {
            // Find the paired dd (if any) to get its style for the wrapper
            let dd_info = if i + 1 < children.len() {
                let next_id = children[i + 1];
                chapter.node(next_id).and_then(|next| {
                    if next.role == Role::DefinitionDescription {
                        Some((next_id, next.style))
                    } else {
                        None
                    }
                })
            } else {
                None
            };

            // Start a wrapper Paragraph for this dt+dd pair
            // Use a neutral style (from dd or default)
            let mut wrapper_elem = ElementStart::new(Role::Paragraph);
            let wrapper_style = if let Some((dd_node_id, dd_style_id)) = dd_info {
                ctx.register_style_id_with_hint(
                    dd_style_id,
                    &chapter.styles,
                    chapter.semantics.class(dd_node_id),
                )
            } else {
                ctx.default_style_symbol
            };
            wrapper_elem.style_symbol = Some(wrapper_style);
            stream.push(KfxToken::StartElement(wrapper_elem));

            // Emit the dt as a Container (with float:left style)
            // Use DefinitionTerm role since it maps to KfxSymbol::Container
            let dt_style = ctx.register_style_id_with_hint(
                child.style,
                &chapter.styles,
                chapter.semantics.class(child_id),
            );
            let mut dt_elem = ElementStart::new(Role::DefinitionTerm);
            dt_elem.style_symbol = Some(dt_style);
            stream.push(KfxToken::StartElement(dt_elem));

            // Emit dt's children wrapped in a Paragraph (like KPR)
            let mut dt_inner = ElementStart::new(Role::Paragraph);
            dt_inner.style_symbol = Some(dt_style);
            stream.push(KfxToken::StartElement(dt_inner));

            for dt_child in chapter.children(child_id) {
                walk_node_for_export(chapter, dt_child, sch, ctx, stream);
            }

            stream.push(KfxToken::EndElement); // end dt inner Paragraph
            stream.push(KfxToken::EndElement); // end dt Container

            // Emit the paired dd content
            if let Some((dd_id, _)) = dd_info {
                // Emit dd's children directly (each is a Paragraph)
                for dd_child in chapter.children(dd_id) {
                    walk_node_for_export(chapter, dd_child, sch, ctx, stream);
                }

                i += 1; // Skip the dd, we've processed it
            }

            // End the wrapper
            stream.push(KfxToken::EndElement);
        } else {
            // Non-dt child (orphan dd or other), emit normally
            walk_node_for_export(chapter, child_id, sch, ctx, stream);
        }

        i += 1;
    }

    stream.push(KfxToken::EndElement);
}

/// Emit inline content (Link, Inline, Text) using the flattening algorithm.
///
/// "Push Down, Emit at Bottom": styles are pushed to the innermost run and
/// emitted there, and the resulting style_events never overlap.
fn emit_inline_content_flat(
    chapter: &Chapter,
    node_id: NodeId,
    sch: &crate::formats::kfx::schema::KfxSchema,
    ctx: &mut ExportContext,
    stream: &mut TokenStream,
) {
    // Flatten the inline subtree into segments with computed state
    let mut segments = Vec::new();
    flatten_inline_content(chapter, node_id, InlineState::default(), &mut segments);

    // Convert segments to tokens
    emit_flattened_segments(segments, chapter, sch, ctx, stream);
}

/// Convert a `TokenStream` into the KFX Ion a storyline's `content_list`
/// holds, splitting structure from text: containers come back as Ion for the
/// storyline entity, and text strings go to `ctx.text_accumulator` for the
/// content entity. A text container takes a `content: {name, index}` reference
/// over inline text, one content entry per element.
///
/// StartSpan/EndSpan become `style_events`: the span stack holds
/// `(start_offset, span_info)` and takes the length at EndSpan.
pub fn tokens_to_ion(tokens: &TokenStream, ctx: &mut ExportContext) -> IonValue {
    let mut stack: Vec<IonBuilder> = vec![IonBuilder::new()];

    // Span stack: (start_byte_offset, SpanStart info)
    // Offset/length for style_events
    let mut span_stack: Vec<(usize, SpanStart)> = Vec::new();

    for token in tokens {
        match token {
            KfxToken::StartElement(elem) => {
                // A border renders on a `type: container` holding a nested
                // `type: text`. A bordered leaf takes that wrapper; a bordered
                // element with block children takes the path below.
                if elem.needs_container_wrapper && !elem.has_block_children {
                    // The outer container carries a writing-mode keyed layout
                    // and the semantic/style fields; the inner text element
                    // carries the content.
                    let mut outer_fields = Vec::new();

                    // Unique container ID for outer wrapper
                    let outer_id = ctx.fragment_ids.next_id();
                    outer_fields.push((sym!(Id), IonValue::Int(outer_id as i64)));

                    // Record this content ID for position_map
                    ctx.record_content_id(outer_id);

                    // Create chapter-start anchor with first content fragment ID (if pending)
                    ctx.resolve_pending_chapter_anchor(outer_id);

                    // Create fragment-based anchor if this element is a link/TOC target
                    if let Some(node_id) = elem.node_id {
                        let has_id = elem.get_semantic(SemanticTarget::Id).is_some();
                        let is_target = ctx.is_registered_target(node_id);
                        if has_id || is_target {
                            ctx.create_anchor_if_needed(node_id, outer_id, 0);
                        }
                    }

                    // Style reference - outer container gets full style with borders
                    let style_sym = elem.style_symbol.unwrap_or(ctx.default_style_symbol);
                    outer_fields.push((sym!(Style), IonValue::Symbol(style_sym)));

                    // Type: container (not text) - this is key for borders to render
                    outer_fields.push((sym!(Type), IonValue::Symbol(KfxSymbol::Container as u64)));

                    // Layout: block-progression axis, keyed to the box's own
                    // writing mode (horizontal for 縦書き), computed in
                    // ir_to_tokens; falls back to the document axis.
                    let layout = elem
                        .container_layout
                        .unwrap_or(ctx.container_layout_symbol() as u64);
                    outer_fields.push((sym!(Layout), IonValue::Symbol(layout)));

                    // Add semantic type annotation if the strategy specifies one
                    if let Some(strategy) = schema().export_strategy(elem.role)
                        && let Some(semantic_type) = strategy.semantic_type()
                    {
                        let field_id = ctx.symbols.get_or_intern("yj.semantics.type");
                        let value_id = ctx.symbols.get_or_intern(semantic_type);
                        outer_fields.push((field_id, IonValue::Symbol(value_id)));
                    }

                    // Add heading level if this is a heading
                    if let Role::Heading(level) = elem.role {
                        outer_fields
                            .push((sym!(YjSemanticsHeadingLevel), IonValue::Int(level as i64)));
                        ctx.record_heading_with_id(level, outer_id);
                    }

                    // Add list_style for ordered lists
                    if elem.role == Role::OrderedList {
                        outer_fields.push((sym!(ListStyle), IonValue::Symbol(sym!(Numeric))));
                    }

                    // Add layout_hints
                    let layout_hint = match elem.role {
                        Role::Heading(_) => Some(KfxSymbol::TreatAsTitle),
                        Role::Figure => Some(KfxSymbol::Figure),
                        Role::Caption => Some(KfxSymbol::Caption),
                        _ => {
                            if let Some(epub_type) = elem.get_semantic(SemanticTarget::EpubType) {
                                let has_title_type = epub_type.split_whitespace().any(|t| {
                                    matches!(
                                        t,
                                        "title"
                                            | "fulltitle"
                                            | "subtitle"
                                            | "covertitle"
                                            | "halftitle"
                                    )
                                });
                                let has_caption_type = epub_type
                                    .split_whitespace()
                                    .any(|t| matches!(t, "caption" | "figcaption"));

                                if has_title_type {
                                    Some(KfxSymbol::TreatAsTitle)
                                } else if has_caption_type {
                                    Some(KfxSymbol::Caption)
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        }
                    };

                    if let Some(hint) = layout_hint {
                        outer_fields.push((
                            sym!(LayoutHints),
                            IonValue::List(vec![IonValue::Symbol(hint as u64)]),
                        ));
                    }

                    // `yj.classification` marks a footnote/endnote body, which Kindle
                    if let Some(classification) = elem
                        .get_semantic(SemanticTarget::EpubType)
                        .and_then(note_classification)
                    {
                        outer_fields
                            .push((sym!(YjClassification), IonValue::Symbol(classification)));
                    }

                    // Add schema-driven attributes from kfx_attrs
                    for (field_id, value_str) in &elem.kfx_attrs {
                        let is_symbol_field = *field_id == sym!(ResourceName)
                            || *field_id == sym!(LinkTo)
                            || value_str.starts_with('#')
                            || value_str.contains('/');

                        if is_symbol_field {
                            let sym_id = ctx.symbols.get_or_intern(value_str);
                            outer_fields.push((*field_id, IonValue::Symbol(sym_id)));
                        } else {
                            outer_fields.push((*field_id, IonValue::String(value_str.clone())));
                        }
                    }

                    // The span belongs to the cell box, which is the outer
                    // container here — the inner text element is its content.
                    push_structural_fields(&mut outer_fields, elem);

                    // Push outer container builder
                    stack.push(IonBuilder::with_fields(outer_fields, outer_id));

                    // Create inner text element
                    let mut inner_fields = Vec::new();

                    // Unique ID for inner text element
                    let inner_id = ctx.fragment_ids.next_id();
                    inner_fields.push((sym!(Id), IonValue::Int(inner_id as i64)));

                    // Record inner content ID too
                    ctx.record_content_id(inner_id);

                    // Inner element uses default style (minimal, no borders)
                    // This matches KPR behavior where inner text has separate style
                    inner_fields.push((sym!(Style), IonValue::Symbol(ctx.default_style_symbol)));

                    // Type: text - inner element holds the actual content
                    inner_fields.push((sym!(Type), IonValue::Symbol(KfxSymbol::Text as u64)));

                    // The inner text builder carries `outer_id`, under which an
                    // anchor inside it navigates to the top-level container.
                    let mut inner_builder = IonBuilder::with_fields(inner_fields, inner_id);
                    inner_builder.is_inner_wrapper_text = true;
                    inner_builder.outer_container_id = Some(outer_id);
                    stack.push(inner_builder);
                } else {
                    // === NORMAL ELEMENT PATH (unchanged) ===
                    let mut fields = Vec::new();

                    // Unique container ID - use the global generator to avoid collisions
                    let container_id = ctx.fragment_ids.next_id();
                    fields.push((sym!(Id), IonValue::Int(container_id as i64)));

                    // The content id a position_map entry resolves a nav target through.
                    ctx.record_content_id(container_id);

                    // Create chapter-start anchor with first content fragment ID (if pending)
                    ctx.resolve_pending_chapter_anchor(container_id);

                    // Create fragment-based anchor if this element is a link/TOC target
                    // Note: Kindle expects offset: 0 for all navigation entries (per reference KFX)
                    // Check both: elements with IDs AND elements that are registered targets (for TOC)
                    if let Some(node_id) = elem.node_id {
                        let has_id = elem.get_semantic(SemanticTarget::Id).is_some();
                        let is_target = ctx.is_registered_target(node_id);
                        if has_id || is_target {
                            ctx.create_anchor_if_needed(node_id, container_id, 0);
                        }
                    }

                    // Style reference - use per-element style if available, else default
                    // Required for text rendering on Kindle
                    let style_sym = elem.style_symbol.unwrap_or(ctx.default_style_symbol);
                    fields.push((sym!(Style), IonValue::Symbol(style_sym)));

                    // A bordered element with block children (a 罫囲み `<div>` of
                    // `<p>` lines) takes `type: container` with an explicit
                    // `layout`; its `<p>` children become the content list.
                    if elem.needs_container_wrapper {
                        fields.push((sym!(Type), IonValue::Symbol(KfxSymbol::Container as u64)));
                        let layout = elem
                            .container_layout
                            .unwrap_or(ctx.container_layout_symbol() as u64);
                        fields.push((sym!(Layout), IonValue::Symbol(layout)));
                    } else if let Some(kfx_type) = schema().kfx_type_for_role(elem.role) {
                        fields.push((sym!(Type), IonValue::Symbol(kfx_type as u64)));
                    }

                    // Add semantic type annotation if the strategy specifies one
                    // (e.g., BlockQuote → yj.semantics.type: block_quote)
                    if let Some(strategy) = schema().export_strategy(elem.role)
                        && let Some(semantic_type) = strategy.semantic_type()
                    {
                        // Both field name and value are local symbols
                        let field_id = ctx.symbols.get_or_intern("yj.semantics.type");
                        let value_id = ctx.symbols.get_or_intern(semantic_type);
                        fields.push((field_id, IonValue::Symbol(value_id)));
                    }

                    // Add heading level if this is a heading
                    if let Role::Heading(level) = elem.role {
                        fields.push((sym!(YjSemanticsHeadingLevel), IonValue::Int(level as i64)));

                        // Record heading position with ACTUAL content fragment ID (Fix for navigation)
                        ctx.record_heading_with_id(level, container_id);
                    }

                    // Add list_style for ordered lists
                    if elem.role == Role::OrderedList {
                        fields.push((sym!(ListStyle), IonValue::Symbol(sym!(Numeric))));
                    }

                    // Add layout_hints based on element role and semantics
                    // This affects Kindle's rendering behavior for headings, figures, and captions
                    let layout_hint = match elem.role {
                        // Headings (h1-h6) → treat_as_title
                        Role::Heading(_) => Some(KfxSymbol::TreatAsTitle),
                        // <figure> → figure
                        Role::Figure => Some(KfxSymbol::Figure),
                        // <figcaption>/<caption> → caption
                        Role::Caption => Some(KfxSymbol::Caption),
                        _ => {
                            // Check epub:type for additional semantic hints
                            if let Some(epub_type) = elem.get_semantic(SemanticTarget::EpubType) {
                                // Check each epub:type value (space-separated)
                                let has_title_type = epub_type.split_whitespace().any(|t| {
                                    matches!(
                                        t,
                                        "title"
                                            | "fulltitle"
                                            | "subtitle"
                                            | "covertitle"
                                            | "halftitle"
                                    )
                                });
                                let has_caption_type = epub_type
                                    .split_whitespace()
                                    .any(|t| matches!(t, "caption" | "figcaption"));

                                if has_title_type {
                                    Some(KfxSymbol::TreatAsTitle)
                                } else if has_caption_type {
                                    Some(KfxSymbol::Caption)
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        }
                    };

                    if let Some(hint) = layout_hint {
                        fields.push((
                            sym!(LayoutHints),
                            IonValue::List(vec![IonValue::Symbol(hint as u64)]),
                        ));
                    }

                    // `yj.classification` marks a footnote/endnote body, which Kindle
                    // shows in a popup at a tap on its noteref link.
                    // when a noteref link is tapped
                    if let Some(classification) = elem
                        .get_semantic(SemanticTarget::EpubType)
                        .and_then(note_classification)
                    {
                        fields.push((sym!(YjClassification), IonValue::Symbol(classification)));
                    }

                    // Add schema-driven attributes from kfx_attrs
                    // The schema handles Image src→resource_name, Link href→link_to, etc.
                    for (field_id, value_str) in &elem.kfx_attrs {
                        // ResourceName and LinkTo are symbols, as is any value
                        // starting `#` or holding `/`. Alt text and the rest
                        // stay strings.
                        let is_symbol_field = *field_id == sym!(ResourceName)
                            || *field_id == sym!(LinkTo)
                            || value_str.starts_with('#')
                            || value_str.contains('/');

                        if is_symbol_field {
                            let sym_id = ctx.symbols.get_or_intern(value_str);
                            fields.push((*field_id, IonValue::Symbol(sym_id)));
                        } else {
                            fields.push((*field_id, IonValue::String(value_str.clone())));
                        }
                    }

                    push_structural_fields(&mut fields, elem);

                    let mut builder = IonBuilder::with_fields(fields, container_id);
                    builder.is_image = elem.role == Role::Image;
                    stack.push(builder);
                }
            }
            KfxToken::EndElement => {
                if let Some(completed) = stack.pop() {
                    let is_inner = completed.is_inner_wrapper_text;
                    let inline_image = completed.is_image;
                    if let Some(parent) = stack.last_mut() {
                        let built = completed.build(ctx);
                        if inline_image && parent.has_real_text_so_far() {
                            // An image closing inside a text-bearing element is
                            // in-run (`（河出<img/>文庫）`): it interleaves into
                            // the parent's content_list at its own position.
                            parent.absorb_inline_image(built);
                        } else {
                            parent.add_child(built);
                        }
                    }

                    // An inner wrapper text element also closes the outer
                    // container (which consumes the same EndElement token)
                    if is_inner
                        && let Some(outer_completed) = stack.pop()
                        && let Some(outer_parent) = stack.last_mut()
                    {
                        outer_parent.add_child(outer_completed.build(ctx));
                    }
                }
            }
            KfxToken::Text(text) => {
                // Append text to the current element's accumulated content
                // This ensures all text within an element is concatenated
                if let Some(current) = stack.last_mut() {
                    current.append_text(text);
                }
            }
            KfxToken::StartSpan(span) => {
                // Push the span onto the stack with current text offset
                // The offset is relative to the current element's accumulated text
                let current_offset = stack.last().map(|b| b.text_len()).unwrap_or(0);

                // Create anchor for inline elements with IDs or that are link/TOC targets
                // For elements inside container wrappers, use the outer container's ID
                if let Some(node_id) = span.node_id
                    && let Some(parent) = stack.last()
                {
                    let has_id = span.get_semantic(SemanticTarget::Id).is_some();
                    let is_target = ctx.is_registered_target(node_id);
                    if has_id || is_target {
                        // Prefer outer_container_id (for wrapped elements) over container_id
                        let target_id = parent.outer_container_id.or(parent.container_id);
                        if let Some(container_id) = target_id {
                            ctx.create_anchor_if_needed(node_id, container_id, current_offset);
                        }
                    }
                }

                span_stack.push((current_offset, span.clone()));
            }
            KfxToken::EndSpan => {
                // Pop the span and calculate its length
                if let Some((start_offset, mut span_info)) = span_stack.pop() {
                    // Calculate length based on accumulated text in the current element
                    let current_offset = stack.last().map(|b| b.text_len()).unwrap_or(0);
                    let length = current_offset.saturating_sub(start_offset);

                    // Update the span with calculated offset and length
                    span_info.offset = start_offset;
                    span_info.length = length;

                    // Add the span as a style_event (if non-empty)
                    // Note: The flattening algorithm ensures spans are non-overlapping
                    // and carry every accumulated attribute merged.
                    if length > 0
                        && let Some(current) = stack.last_mut()
                    {
                        current.add_style_event(span_info, ctx);
                    }
                }
            }
        }
    }

    // Return the root children as a list (the storyline content_list)
    if let Some(root) = stack.pop() {
        root.build(ctx)
    } else {
        IonValue::List(vec![])
    }
}

/// Builder for constructing Ion structures from tokens.
/// True when a built content_list child is an image element struct
/// (`type: image`). Tells in-run images apart from other children
/// when deciding whether to interleave.
fn is_image_struct(value: &IonValue) -> bool {
    matches!(value, IonValue::Struct(fields) if fields.iter().any(|(id, val)| {
        *id == sym!(Type)
            && matches!(val, IonValue::Symbol(s) if *s == KfxSymbol::Image as u64)
    }))
}

struct IonBuilder {
    fields: Vec<(u64, IonValue)>,
    children: Vec<IonValue>,
    /// Accumulated text content for this element (concatenated during build)
    accumulated_text: String,
    /// Character count of accumulated text (for style event offsets)
    /// KFX uses character offsets, not byte offsets
    accumulated_char_count: usize,
    /// Collected style events for this element (inline spans)
    style_events: Vec<IonValue>,
    /// Container ID for this element (set during StartElement, used for length tracking)
    container_id: Option<u64>,
    /// True if this is an inner text element inside a container wrapper.
    /// EndElement for this builder takes an extra EndElement
    /// to close the outer container.
    is_inner_wrapper_text: bool,
    /// For inner wrapper text elements, stores the outer container's ID.
    /// Anchors inside wrapped elements should use this ID for correct TOC navigation.
    outer_container_id: Option<u64>,
    /// Completed inline-content runs, in document order: bare text strings
    /// interleaved with `render: inline` image structs — Amazon's shape for a
    /// paragraph with in-run images (`（河出<img/>文庫）`). Non-empty only when
    /// an inline image split this element's text; `accumulated_text` then
    /// holds the run since the last split, and `build` emits `content_list`
    /// from these runs, with no text externalized to the content entity.
    inline_runs: Vec<IonValue>,
    /// Count of images absorbed into `inline_runs`. Each occupies ONE
    /// character position in the style_event offset space (the importer's
    /// counting rule), but position/location spans track it under the
    /// image's own eid — `build` subtracts this from the recorded length.
    inline_image_count: usize,
    /// True when this element is a plain (non-wrapped) image — lets
    /// EndElement route it into a text-bearing parent's inline runs.
    is_image: bool,
}

impl IonBuilder {
    fn new() -> Self {
        Self {
            fields: Vec::new(),
            children: Vec::new(),
            accumulated_text: String::new(),
            accumulated_char_count: 0,
            style_events: Vec::new(),
            container_id: None,
            is_inner_wrapper_text: false,
            outer_container_id: None,
            inline_runs: Vec::new(),
            inline_image_count: 0,
            is_image: false,
        }
    }

    fn with_fields(fields: Vec<(u64, IonValue)>, container_id: u64) -> Self {
        Self {
            fields,
            children: Vec::new(),
            accumulated_text: String::new(),
            accumulated_char_count: 0,
            style_events: Vec::new(),
            container_id: Some(container_id),
            is_inner_wrapper_text: false,
            outer_container_id: None,
            inline_runs: Vec::new(),
            inline_image_count: 0,
            is_image: false,
        }
    }

    fn add_child(&mut self, child: IonValue) {
        self.children.push(child);
    }

    /// Append text to this element's accumulated content.
    /// Returns the character offset where this text starts (for span tracking).
    /// KFX style events use character offsets, not byte offsets.
    fn append_text(&mut self, text: &str) -> usize {
        // Text arriving after image-only children puts those images in-run
        // before it (`<p><img/>text…`): they migrate into the inline
        // interleave, which keeps their position.
        if !self.children.is_empty()
            && !text.chars().all(|c| c == '\u{200B}')
            && self.children.iter().all(is_image_struct)
        {
            let migrated: Vec<IonValue> = self.children.drain(..).collect();
            for img in migrated {
                self.absorb_inline_image(img);
            }
        }
        let offset = self.accumulated_char_count;
        self.accumulated_text.push_str(text);
        self.accumulated_char_count += text.chars().count();
        offset
    }

    /// True when the element carries real inline content — the
    /// trigger for interleaving an in-run image over appending it as
    /// a block child. Zero-width spaces don't count: they are anchor
    /// carriers, and an anchored standalone image (`<a id><img/></a>`) must
    /// keep the plain image-child shape.
    fn has_real_text_so_far(&self) -> bool {
        !self.inline_runs.is_empty() || self.accumulated_text.chars().any(|c| c != '\u{200B}')
    }

    /// Absorb an image struct into the inline interleave: flush the pending
    /// text as a bare content_list string, append the image (stamped
    /// `render: inline`), and advance the offset space by ONE character —
    /// the position the importer counts for an in-run image, keeping
    /// style_event offsets after the image aligned.
    fn absorb_inline_image(&mut self, mut image: IonValue) {
        if !self.accumulated_text.is_empty() {
            let run = std::mem::take(&mut self.accumulated_text);
            self.inline_runs.push(IonValue::String(run));
        }
        if let IonValue::Struct(ref mut fields) = image
            && !fields.iter().any(|(id, _)| *id == sym!(Render))
        {
            fields.push((sym!(Render), IonValue::Symbol(KfxSymbol::Inline as u64)));
        }
        self.inline_runs.push(image);
        self.inline_image_count += 1;
        self.accumulated_char_count += 1;
    }

    /// Get the current accumulated text length in characters.
    /// KFX style events use character offsets, not byte offsets.
    fn text_len(&self) -> usize {
        self.accumulated_char_count
    }

    /// Add a style event (inline span) to this element.
    ///
    /// Converts SpanStart into KFX style_event Ion struct:
    /// { offset: N, length: N, style: $symbol [, link_to: $anchor] }
    fn add_style_event(&mut self, span: SpanStart, ctx: &mut ExportContext) {
        let mut event_fields = Vec::new();

        // Offset and length (required)
        event_fields.push((sym!(Offset), IonValue::Int(span.offset as i64)));
        event_fields.push((sym!(Length), IonValue::Int(span.length as i64)));

        // Style reference (required for rendering)
        if let Some(style_sym) = span.style_symbol {
            event_fields.push((sym!(Style), IonValue::Symbol(style_sym)));
        } else {
            // Use default style if no specific style
            event_fields.push((sym!(Style), IonValue::Symbol(ctx.default_style_symbol)));
        }

        // Add span-specific attributes (e.g., link_to for links, yj.display for noterefs)
        for (field_id, value_str) in &span.kfx_attrs {
            if *field_id == sym!(LinkTo) {
                // LinkTo is always a symbol reference (anchor symbol)
                let sym_id = ctx.symbols.get_or_intern(value_str);
                event_fields.push((*field_id, IonValue::Symbol(sym_id)));
            } else if *field_id == sym!(YjDisplay) {
                // YjDisplay value is a symbol ID (e.g., YjNote = 617)
                if let Ok(sym_id) = value_str.parse::<u64>() {
                    event_fields.push((*field_id, IonValue::Symbol(sym_id)));
                }
            } else if *field_id == sym!(RubyName) {
                // RubyName is a symbol reference to a ruby_content fragment kfx_id
                let sym_id = ctx.symbols.get_or_intern(value_str);
                event_fields.push((*field_id, IonValue::Symbol(sym_id)));
            } else if *field_id == sym!(RubyId) {
                // RubyId is a 1-indexed integer entry within the fragment
                if let Ok(n) = value_str.parse::<i64>() {
                    event_fields.push((*field_id, IonValue::Int(n)));
                }
            } else {
                event_fields.push((*field_id, IonValue::String(value_str.clone())));
            }
        }

        self.style_events.push(IonValue::Struct(event_fields));
    }

    /// Finalize and build the Ion struct, creating content reference if text was accumulated.
    fn build(mut self, ctx: &mut ExportContext) -> IonValue {
        // KFX storylines are flat lists of elements, not nested structs
        // Each element is a struct with type, content reference, and possibly nested content_list
        if !self.fields.is_empty() {
            // The content id's text length in characters, which is what
            // location_map divides by. An inline image occupies an offset-space
            // slot under its own eid, outside this element's recorded length.
            if let Some(container_id) = self.container_id {
                ctx.record_content_length(
                    container_id,
                    self.accumulated_char_count - self.inline_image_count,
                );
            }

            if !self.inline_runs.is_empty() {
                // Interleave shape (in-run images): bare strings and
                // `render: inline` image structs mixed in `content_list`, no
                // content entity, each image one character of offset space.
                if !self.accumulated_text.is_empty() {
                    self.inline_runs
                        .push(IonValue::String(std::mem::take(&mut self.accumulated_text)));
                }
                if !self.style_events.is_empty() {
                    self.fields
                        .push((sym!(StyleEvents), IonValue::List(self.style_events)));
                }
                let mut list = std::mem::take(&mut self.inline_runs);
                // Stray block children (none expected in a text run) keep
                // their trailing position.
                list.append(&mut self.children);
                self.fields.push((sym!(ContentList), IonValue::List(list)));
                return IonValue::Struct(self.fields);
            }

            // If this element has accumulated text, create ONE content reference
            // Skip if the only content is zero-width spaces (anchor markers from empty ID elements)
            // These interfere with image display when mixed with image children
            let has_real_text = self.accumulated_text.chars().any(|c| c != '\u{200B}');
            if has_real_text {
                let (content_idx, _offset) = ctx.append_text(&self.accumulated_text);
                let content_ref = IonValue::Struct(vec![
                    (sym!(Name), IonValue::Symbol(ctx.current_content_name)),
                    (sym!(Index), IonValue::Int(content_idx as i64)),
                ]);
                self.fields.push((sym!(Content), content_ref));
            }

            // style_events go in beside real text: they carry character offsets
            // into the content.
            if !self.style_events.is_empty() && has_real_text {
                self.fields
                    .push((sym!(StyleEvents), IonValue::List(self.style_events)));
            }

            // Add nested children as content_list if present
            if !self.children.is_empty() {
                self.fields
                    .push((sym!(ContentList), IonValue::List(self.children)));
            }

            IonValue::Struct(self.fields)
        } else if !self.children.is_empty() {
            // Root level: return list of children
            IonValue::List(self.children)
        } else {
            IonValue::Null
        }
    }
}

/// Build a storyline Ion structure from an IR chapter.
///
/// **Note**: Internal — use `build_chapter_entities` for the full
/// three-entity architecture (Content, Storyline, Section).
pub fn build_storyline_ion(chapter: &Chapter, ctx: &mut ExportContext) -> IonValue {
    let tokens = ir_to_tokens(chapter, ctx);
    tokens_to_ion(&tokens, ctx)
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    /// Empty symbol table for tests that carry no doc-local symbols.
    fn no_symbols() -> SymbolTable {
        SymbolTable::new(0, Vec::new())
    }
    use crate::model::{GlobalNodeId, Role};
    use crate::style::BorderStyle;

    #[test]
    fn test_is_short_ascii_digit_run() {
        // Chapter numbers / short counts → tate-chu-yoko candidates.
        assert!(is_short_ascii_digit_run("1"));
        assert!(is_short_ascii_digit_run("13"));
        // Too long (years), non-digit, empty, or CJK → not combined.
        assert!(!is_short_ascii_digit_run("100"));
        assert!(!is_short_ascii_digit_run("1999"));
        assert!(!is_short_ascii_digit_run("1a"));
        assert!(!is_short_ascii_digit_run(""));
        assert!(!is_short_ascii_digit_run("一")); // CJK numeral, already upright
        assert!(!is_short_ascii_digit_run(" 1")); // stray whitespace disqualifies
    }

    #[test]
    fn test_tokenize_creates_proper_structure() {
        // Test that tokenization produces expected token sequence
        let mut stream = TokenStream::new();
        stream.start_element(Role::Paragraph);
        stream.text("Hello");
        stream.end_element();

        let chapter = build_ir_from_tokens(&stream, &no_symbols(), None, |_, _| None);
        assert_eq!(chapter.node_count(), 3); // root + para + text
    }

    #[test]
    fn test_build_ir_with_image() {
        let mut stream = TokenStream::new();
        let mut semantics = HashMap::new();
        semantics.insert(SemanticTarget::Src, "cover.jpg".to_string());

        stream.push(KfxToken::StartElement(ElementStart {
            role: Role::Image,
            node_id: None,
            id: Some(123),
            semantics,
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
        }));
        stream.end_element();

        let chapter = build_ir_from_tokens(&stream, &no_symbols(), None, |_, _| None);

        let children: Vec<_> = chapter.children(chapter.root()).collect();
        assert_eq!(children.len(), 1);

        let image_node = chapter.node(children[0]).unwrap();
        assert_eq!(image_node.role, Role::Image);
        assert_eq!(chapter.semantics.src(children[0]), Some("cover.jpg"));
    }

    #[test]
    fn test_build_ir_with_text_content() {
        let mut stream = TokenStream::new();
        stream.push(KfxToken::StartElement(ElementStart {
            role: Role::Paragraph,
            node_id: None,
            id: None,
            semantics: HashMap::new(),
            content_ref: Some(ContentRef {
                name: "content_1".to_string(),
                index: 0,
            }),
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
        }));
        stream.end_element();

        let chapter = build_ir_from_tokens(&stream, &no_symbols(), None, |name, idx| {
            if name == "content_1" && idx == 0 {
                Some("Hello, world!".to_string())
            } else {
                None
            }
        });

        assert_eq!(chapter.node_count(), 3); // root + para + text
        let para_id = chapter.children(chapter.root()).next().unwrap();
        let text_id = chapter.children(para_id).next().unwrap();
        let text_node = chapter.node(text_id).unwrap();
        assert_eq!(chapter.text(text_node.text), "Hello, world!");
    }

    #[test]
    fn test_build_ir_with_heading() {
        let mut stream = TokenStream::new();
        stream.push(KfxToken::StartElement(ElementStart {
            role: Role::Heading(2),
            node_id: None,
            id: None,
            semantics: HashMap::new(),
            content_ref: Some(ContentRef {
                name: "content_1".to_string(),
                index: 0,
            }),
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
            // A `$790` level alone never promotes (calibre's rule);
            // the "heading" hint must accompany it.
            layout_hints: vec!["heading".to_string()],
            heading_level: Some("2".to_string()),
            column_span: None,
            row_span: None,
            column_format: Vec::new(),
            list_start: None,
        }));
        stream.end_element();

        let chapter = build_ir_from_tokens(&stream, &no_symbols(), None, |_, _| {
            Some("Chapter 1".to_string())
        });

        let heading_id = chapter.children(chapter.root()).next().unwrap();
        let heading = chapter.node(heading_id).unwrap();
        assert_eq!(heading.role, Role::Heading(2));
    }

    #[test]
    fn test_build_ir_with_link_span() {
        let mut stream = TokenStream::new();
        let mut span_semantics = HashMap::new();
        span_semantics.insert(SemanticTarget::Href, "chapter2".to_string());

        stream.push(KfxToken::StartElement(ElementStart {
            role: Role::Paragraph,
            node_id: None,
            id: None,
            semantics: HashMap::new(),
            content_ref: Some(ContentRef {
                name: "content_1".to_string(),
                index: 0,
            }),
            style_events: vec![SpanStart {
                role: Role::Link,
                node_id: None,
                semantics: span_semantics,
                offset: 7,
                length: 5,
                style_symbol: None,
                ruby_annotation: None,
                ruby_pairs: Vec::new(),
                kfx_attrs: Vec::new(),
            }],
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
        stream.end_element();

        // Text is "Hello, world!" - span at offset 7, length 5 = "world"
        let chapter = build_ir_from_tokens(&stream, &no_symbols(), None, |_, _| {
            Some("Hello, world!".to_string())
        });

        // Should have: root -> para -> [text("Hello, "), link("world"), text("!")]
        let para_id = chapter.children(chapter.root()).next().unwrap();
        let children: Vec<_> = chapter.children(para_id).collect();
        assert_eq!(children.len(), 3);

        // First: plain text "Hello, "
        let first = chapter.node(children[0]).unwrap();
        assert_eq!(first.role, Role::Text);
        assert_eq!(chapter.text(first.text), "Hello, ");

        // Second: link containing "world"
        let link = chapter.node(children[1]).unwrap();
        assert_eq!(link.role, Role::Link);
        assert_eq!(chapter.semantics.href(children[1]), Some("chapter2"));

        // Third: plain text "!"
        let last = chapter.node(children[2]).unwrap();
        assert_eq!(last.role, Role::Text);
        assert_eq!(chapter.text(last.text), "!");
    }

    /// A paragraph whose text arrives as interleave runs (no content ref):
    /// its style_events offset into the joined run space where the nested
    /// child element counts ONE position, and apply to the runs they overlap.
    #[test]
    fn test_interleave_style_events_apply_to_runs() {
        let mut stream = TokenStream::new();
        stream.push(KfxToken::StartElement(ElementStart {
            role: Role::Paragraph,
            node_id: None,
            id: Some(50),
            semantics: HashMap::new(),
            content_ref: None,
            // 「いや(3) + child(1) + 　お(2) → 互 sits at offset 6.
            style_events: vec![SpanStart {
                role: Role::Ruby,
                node_id: None,
                semantics: HashMap::new(),
                offset: 6,
                length: 1,
                style_symbol: None,
                ruby_annotation: Some("たが".to_string()),
                ruby_pairs: Vec::new(),
                kfx_attrs: Vec::new(),
            }],
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
        stream.push(KfxToken::Text("「いや".to_string()));
        // Nested tate-chu-yoko run — its own text is 2 chars long but it
        // occupies one position in the parent's event space.
        stream.push(KfxToken::StartElement(ElementStart {
            role: Role::Paragraph,
            node_id: None,
            id: Some(51),
            semantics: HashMap::new(),
            content_ref: Some(ContentRef {
                name: "content_1".to_string(),
                index: 0,
            }),
            style_events: Vec::new(),
            kfx_attrs: Vec::new(),
            style_symbol: None,
            style_name: None,
            needs_container_wrapper: false,
            has_block_children: false,
            container_layout: None,
            inline_style: Vec::new(),
            render_inline: true,
            is_image: false,
            layout_hints: Vec::new(),
            heading_level: None,
            column_span: None,
            row_span: None,
            column_format: Vec::new(),
            list_start: None,
        }));
        stream.end_element();
        stream.push(KfxToken::Text("　お互いです".to_string()));
        stream.end_element();

        let chapter =
            build_ir_from_tokens(&stream, &no_symbols(), None, |_, _| Some("!!".to_string()));

        let para_id = chapter.children(chapter.root()).next().unwrap();
        let children: Vec<_> = chapter.children(para_id).collect();
        let roles: Vec<Role> = children
            .iter()
            .map(|c| chapter.node(*c).unwrap().role)
            .collect();
        // [「いや][!! run][　お][ruby 互][です]
        assert_eq!(
            roles,
            vec![Role::Text, Role::Inline, Role::Text, Role::Ruby, Role::Text]
        );
        let ruby = children[3];
        let base = chapter.children(ruby).next().unwrap();
        let base_node = chapter.node(base).unwrap();
        assert_eq!(base_node.role, Role::Text);
        assert_eq!(chapter.text(base_node.text), "互");
        let rt = chapter.children(ruby).nth(1).unwrap();
        assert_eq!(chapter.node(rt).unwrap().role, Role::RubyText);
        let tail = chapter.node(children[4]).unwrap();
        assert_eq!(chapter.text(tail.text), "いです");
    }

    /// Offset anchors into an interleave element resolve in event space:
    /// nested child elements count one position and restored ruby BASE text
    /// counts (annotation text does not).
    #[test]
    fn test_interleave_offset_anchor_stamps_in_event_space() {
        let mut table = AnchorTable::default();
        // Anchor at (eid 50, offset 8): 「いや(3) + child(1) + 　お(2) +
        // 互 base(1) + い(1) → lands before "です".
        let pos = IonValue::Struct(vec![(
            KfxSymbol::Position as u64,
            IonValue::Struct(vec![
                (KfxSymbol::Id as u64, IonValue::Int(50)),
                (KfxSymbol::Offset as u64, IonValue::Int(8)),
            ]),
        )]);
        table.register_anchor_fields("mid", pos.as_struct().unwrap());

        let mut stream = TokenStream::new();
        stream.push(KfxToken::StartElement(ElementStart {
            role: Role::Paragraph,
            node_id: None,
            id: Some(50),
            semantics: HashMap::new(),
            content_ref: None,
            style_events: vec![SpanStart {
                role: Role::Ruby,
                node_id: None,
                semantics: HashMap::new(),
                offset: 6,
                length: 1,
                style_symbol: None,
                ruby_annotation: Some("たが".to_string()),
                ruby_pairs: Vec::new(),
                kfx_attrs: Vec::new(),
            }],
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
        stream.push(KfxToken::Text("「いや".to_string()));
        stream.push(KfxToken::StartElement(ElementStart {
            role: Role::Paragraph,
            node_id: None,
            id: Some(51),
            semantics: HashMap::new(),
            content_ref: Some(ContentRef {
                name: "content_1".to_string(),
                index: 0,
            }),
            style_events: Vec::new(),
            kfx_attrs: Vec::new(),
            style_symbol: None,
            style_name: None,
            needs_container_wrapper: false,
            has_block_children: false,
            container_layout: None,
            inline_style: Vec::new(),
            render_inline: true,
            is_image: false,
            layout_hints: Vec::new(),
            heading_level: None,
            column_span: None,
            row_span: None,
            column_format: Vec::new(),
            list_start: None,
        }));
        stream.end_element();
        stream.push(KfxToken::Text("　お互いです".to_string()));
        stream.end_element();

        let chapter =
            build_ir_from_tokens_anchored(&stream, &no_symbols(), None, Some(&table), |_, _| {
                Some("!!".to_string())
            });

        // The anchor splits the tail run between "い" and "です" and
        // locates: calibre's own walk stops at the ruby and never
        // reaches offset 8.
        let stamped = table.id_at(50, 8).unwrap();
        let para_id = chapter.children(chapter.root()).next().unwrap();
        let mut found_between = false;
        for c in chapter.children(para_id).collect::<Vec<_>>() {
            if chapter.semantics.id(c) == Some(stamped.as_str()) {
                let next = chapter.node(c).unwrap().next_sibling;
                let next_text = next
                    .and_then(|n| chapter.node(n))
                    .map(|n| chapter.text(n.text));
                assert_eq!(next_text, Some("です"));
                found_between = true;
            }
        }
        assert!(found_between, "anchor did not stamp in event space");
    }

    #[test]
    fn test_char_to_byte_offset() {
        let text = "Hello ὑπόληψις world";

        // ASCII chars: byte offset = char offset
        assert_eq!(char_to_byte_offset(text, 0), 0); // 'H'
        assert_eq!(char_to_byte_offset(text, 5), 5); // ' '

        // The Greek run starts at char 6, byte 6, three bytes to the
        // character: char 7 is byte 9, char 14 the space at byte 23.
        assert_eq!(char_to_byte_offset(text, 6), 6); // 'ὑ'
        assert_eq!(char_to_byte_offset(text, 7), 9); // 'π'
        assert_eq!(char_to_byte_offset(text, 14), 23); // ' ' after Greek

        // Past end returns text.len()
        assert_eq!(char_to_byte_offset(text, 100), text.len());
    }

    #[test]
    fn test_apply_semantics_generic() {
        let mut chapter = Chapter::new();
        let node = Node::new(Role::Image);
        let node_id = chapter.alloc_node(node);

        let mut semantics = HashMap::new();
        semantics.insert(SemanticTarget::Src, "image.jpg".to_string());
        semantics.insert(SemanticTarget::Alt, "An image".to_string());

        apply_semantics_to_node(&mut chapter, node_id, &semantics);

        assert_eq!(chapter.semantics.src(node_id), Some("image.jpg"));
        assert_eq!(chapter.semantics.alt(node_id), Some("An image"));
    }

    // ========================================================================
    // Export tests
    // ========================================================================

    #[test]
    fn test_ir_to_tokens_basic() {
        let mut chapter = Chapter::new();

        // Create a text node with content
        let text_range = chapter.append_text("Hello");
        let mut text_node = Node::new(Role::Text);
        text_node.text = text_range;
        let text_id = chapter.alloc_node(text_node);
        chapter.append_child(chapter.root(), text_id);

        let mut ctx = ExportContext::new();
        let tokens = ir_to_tokens(&chapter, &mut ctx);

        // Should have tokens for the text node
        assert!(!tokens.is_empty());
    }

    #[test]
    fn test_build_storyline_ion() {
        let mut chapter = Chapter::new();

        // Create a paragraph with a text child
        let para = Node::new(Role::Paragraph);
        let para_id = chapter.alloc_node(para);
        chapter.append_child(chapter.root(), para_id);

        let text_range = chapter.append_text("Test content");
        let mut text_node = Node::new(Role::Text);
        text_node.text = text_range;
        let text_id = chapter.alloc_node(text_node);
        chapter.append_child(para_id, text_id);

        let mut ctx = ExportContext::new();
        let ion = build_storyline_ion(&chapter, &mut ctx);

        // Should produce some Ion structure
        assert!(!matches!(ion, IonValue::Null));
    }

    /// Field lookup on an element struct.
    fn struct_field(ion: &IonValue, id: KfxSymbol) -> Option<&IonValue> {
        match ion {
            IonValue::Struct(fields) => fields
                .iter()
                .find(|(fid, _)| *fid == id as u64)
                .map(|(_, v)| v),
            _ => None,
        }
    }

    fn has_symbol_field(ion: &IonValue, id: KfxSymbol, value: KfxSymbol) -> bool {
        matches!(
            struct_field(ion, id),
            Some(IonValue::Symbol(s)) if *s == value as u64
        )
    }

    #[test]
    fn test_inline_image_interleaves_in_content_list() {
        // `<p>（河出<img/>文庫）</p>` — the mid-run image must land BETWEEN
        // the two text runs as a `render: inline` struct (Amazon's shape),
        // not be appended after externalized text.
        let mut chapter = Chapter::new();

        let para_id = chapter.alloc_node(Node::new(Role::Paragraph));
        chapter.append_child(chapter.root(), para_id);

        let before = chapter.append_text("（河出");
        let mut t1 = Node::new(Role::Text);
        t1.text = before;
        let t1_id = chapter.alloc_node(t1);
        chapter.append_child(para_id, t1_id);

        let img_id = chapter.alloc_node(Node::new(Role::Image));
        chapter.semantics.set_src(img_id, "images/gaiji.jpg");
        chapter.append_child(para_id, img_id);

        let after = chapter.append_text("文庫）");
        let mut t2 = Node::new(Role::Text);
        t2.text = after;
        let t2_id = chapter.alloc_node(t2);
        chapter.append_child(para_id, t2_id);

        let mut ctx = ExportContext::new();
        let ion = build_storyline_ion(&chapter, &mut ctx);

        let IonValue::List(elements) = ion else {
            panic!("storyline root must be a list");
        };
        let para = &elements[0];
        assert!(
            struct_field(para, KfxSymbol::Content).is_none(),
            "interleaved paragraph must not externalize text to the content entity"
        );
        let Some(IonValue::List(content_list)) = struct_field(para, KfxSymbol::ContentList) else {
            panic!("paragraph must carry a content_list");
        };
        assert_eq!(content_list.len(), 3, "string, image, string");
        assert!(
            matches!(&content_list[0], IonValue::String(s) if s == "（河出"),
            "run before the image, got {:?}",
            content_list[0]
        );
        assert!(
            has_symbol_field(&content_list[1], KfxSymbol::Type, KfxSymbol::Image),
            "middle entry must be the image"
        );
        assert!(
            has_symbol_field(&content_list[1], KfxSymbol::Render, KfxSymbol::Inline),
            "in-run image must be stamped render: inline"
        );
        assert!(
            matches!(&content_list[2], IonValue::String(s) if s == "文庫）"),
            "run after the image, got {:?}",
            content_list[2]
        );
    }

    #[test]
    fn test_inline_image_counts_one_position_in_style_events() {
        // `<p>pre<a href>link<img/>tail</a>post</p>` — the image occupies ONE
        // character of offset space, and the link span after it starts one
        // position later ("pre"=3, "link"=4, image=1 → tail at offset 8).
        let mut chapter = Chapter::new();

        let para_id = chapter.alloc_node(Node::new(Role::Paragraph));
        chapter.append_child(chapter.root(), para_id);

        let pre = chapter.append_text("pre");
        let mut t = Node::new(Role::Text);
        t.text = pre;
        let t_id = chapter.alloc_node(t);
        chapter.append_child(para_id, t_id);

        let link_id = chapter.alloc_node(Node::new(Role::Link));
        chapter.semantics.set_href(link_id, "https://example.com/x");
        chapter.append_child(para_id, link_id);

        let ltext = chapter.append_text("link");
        let mut lt = Node::new(Role::Text);
        lt.text = ltext;
        let lt_id = chapter.alloc_node(lt);
        chapter.append_child(link_id, lt_id);

        let img_id = chapter.alloc_node(Node::new(Role::Image));
        chapter.semantics.set_src(img_id, "images/gaiji.jpg");
        chapter.append_child(link_id, img_id);

        let ttext = chapter.append_text("tail");
        let mut tt = Node::new(Role::Text);
        tt.text = ttext;
        let tt_id = chapter.alloc_node(tt);
        chapter.append_child(link_id, tt_id);

        let post = chapter.append_text("post");
        let mut pt = Node::new(Role::Text);
        pt.text = post;
        let pt_id = chapter.alloc_node(pt);
        chapter.append_child(para_id, pt_id);

        let mut ctx = ExportContext::new();
        let ion = build_storyline_ion(&chapter, &mut ctx);

        let IonValue::List(elements) = ion else {
            panic!("storyline root must be a list");
        };
        let para = &elements[0];
        let Some(IonValue::List(events)) = struct_field(para, KfxSymbol::StyleEvents) else {
            panic!("link spans must produce style_events");
        };
        let spans: Vec<(i64, i64)> = events
            .iter()
            .map(|e| {
                let Some(IonValue::Int(off)) = struct_field(e, KfxSymbol::Offset) else {
                    panic!("event offset");
                };
                let Some(IonValue::Int(len)) = struct_field(e, KfxSymbol::Length) else {
                    panic!("event length");
                };
                (*off, *len)
            })
            .collect();
        assert!(
            spans.contains(&(3, 4)),
            "link span before the image at offset 3, got {spans:?}"
        );
        assert!(
            spans.contains(&(8, 4)),
            "link span after the image shifted by the image's slot, got {spans:?}"
        );
    }

    #[test]
    fn test_image_only_paragraph_keeps_block_shape() {
        // `<p><img/></p>` with no surrounding text keeps the plain
        // image-child shape — no interleave, no render: inline.
        let mut chapter = Chapter::new();

        let para_id = chapter.alloc_node(Node::new(Role::Paragraph));
        chapter.append_child(chapter.root(), para_id);

        let img_id = chapter.alloc_node(Node::new(Role::Image));
        chapter.semantics.set_src(img_id, "images/plate.jpg");
        chapter.append_child(para_id, img_id);

        let mut ctx = ExportContext::new();
        let ion = build_storyline_ion(&chapter, &mut ctx);

        let IonValue::List(elements) = ion else {
            panic!("storyline root must be a list");
        };
        let para = &elements[0];
        let Some(IonValue::List(content_list)) = struct_field(para, KfxSymbol::ContentList) else {
            panic!("paragraph must carry the image child");
        };
        assert_eq!(content_list.len(), 1);
        assert!(has_symbol_field(
            &content_list[0],
            KfxSymbol::Type,
            KfxSymbol::Image
        ));
        assert!(
            struct_field(&content_list[0], KfxSymbol::Render).is_none(),
            "a block image must not be stamped render: inline"
        );
    }

    #[test]
    fn test_captioned_figure_migrates_image_into_content_list() {
        // `<div class="pic"><img/>caption</div>`: image first, caption after.
        // The image migrates into the interleave, giving `content_list =
        // [image(render: inline), "caption"]` and no `content` field.
        let mut chapter = Chapter::new();

        let para_id = chapter.alloc_node(Node::new(Role::Paragraph));
        chapter.append_child(chapter.root(), para_id);

        // Image child first — no preceding text.
        let img_id = chapter.alloc_node(Node::new(Role::Image));
        chapter.semantics.set_src(img_id, "images/figure.jpg");
        chapter.append_child(para_id, img_id);

        // Caption text after the image.
        let caption = chapter.append_text("図の説明");
        let mut cap = Node::new(Role::Text);
        cap.text = caption;
        let cap_id = chapter.alloc_node(cap);
        chapter.append_child(para_id, cap_id);

        let mut ctx = ExportContext::new();
        let ion = build_storyline_ion(&chapter, &mut ctx);

        let IonValue::List(elements) = ion else {
            panic!("storyline root must be a list");
        };
        let para = &elements[0];
        assert!(
            struct_field(para, KfxSymbol::Content).is_none(),
            "captioned figure must NOT externalize the caption to a content entity \
             (that strands the image in an ignored content_list)"
        );
        let Some(IonValue::List(content_list)) = struct_field(para, KfxSymbol::ContentList) else {
            panic!("figure must carry a content_list");
        };
        assert_eq!(
            content_list.len(),
            2,
            "content_list must interleave [image, caption], got {content_list:?}"
        );
        assert!(
            has_symbol_field(&content_list[0], KfxSymbol::Type, KfxSymbol::Image),
            "the image must survive as the first content_list entry"
        );
        assert!(
            has_symbol_field(&content_list[0], KfxSymbol::Render, KfxSymbol::Inline),
            "a migrated in-run image is stamped render: inline (Amazon's shape)"
        );
        assert!(
            matches!(&content_list[1], IonValue::String(s) if s == "図の説明"),
            "the caption run follows the image, got {:?}",
            content_list[1]
        );
    }

    #[test]
    fn test_tokens_to_ion_empty() {
        let tokens = TokenStream::new();
        let mut ctx = ExportContext::new();
        let ion = tokens_to_ion(&tokens, &mut ctx);

        // Empty tokens should produce an empty list or null
        assert!(
            matches!(ion, IonValue::List(_)) || matches!(ion, IonValue::Null),
            "expected List or Null, got {:?}",
            ion
        );
    }

    #[test]
    fn test_heading_level_export() {
        use crate::formats::kfx::symbols::KfxSymbol;

        let mut chapter = Chapter::new();

        // Create an H2 heading
        let h2 = Node::new(Role::Heading(2));
        let h2_id = chapter.alloc_node(h2);
        chapter.append_child(chapter.root(), h2_id);

        // Add text content
        let text_range = chapter.append_text("Chapter Title");
        let mut text_node = Node::new(Role::Text);
        text_node.text = text_range;
        let text_id = chapter.alloc_node(text_node);
        chapter.append_child(h2_id, text_id);

        let mut ctx = ExportContext::new();
        let ion = build_storyline_ion(&chapter, &mut ctx);

        // Find the heading container in the output and verify yj.semantics.heading_level = 2
        fn find_heading_level(ion: &IonValue) -> Option<i64> {
            match ion {
                IonValue::Struct(fields) => {
                    for (field_id, value) in fields {
                        if *field_id == KfxSymbol::YjSemanticsHeadingLevel as u64
                            && let IonValue::Int(level) = value
                        {
                            return Some(*level);
                        }
                    }
                    // Check content_list (children in KFX)
                    for (field_id, value) in fields {
                        if *field_id == KfxSymbol::ContentList as u64
                            && let Some(level) = find_heading_level(value)
                        {
                            return Some(level);
                        }
                    }
                }
                IonValue::List(items) => {
                    for item in items {
                        if let Some(level) = find_heading_level(item) {
                            return Some(level);
                        }
                    }
                }
                _ => {}
            }
            None
        }

        let heading_level = find_heading_level(&ion);
        assert_eq!(
            heading_level,
            Some(2),
            "Expected yj.semantics.heading_level = 2, got {:?}",
            heading_level
        );
    }

    #[test]
    fn test_style_event_offsets_use_char_count() {
        // KFX style events use character offsets, not byte offsets.
        // Greek characters are multi-byte in UTF-8, and this verifies
        // characters count, not bytes.
        let mut builder = IonBuilder::new();

        // "Hello " = 6 chars, 6 bytes
        builder.append_text("Hello ");
        assert_eq!(builder.text_len(), 6);

        // "ὑπόληψις" = 8 chars, 17 bytes in UTF-8
        let greek_offset = builder.append_text("ὑπόληψις");
        assert_eq!(greek_offset, 6, "Greek text should start at char offset 6");
        assert_eq!(builder.text_len(), 14, "Total should be 14 chars (6 + 8)");

        // Verify byte length differs from char count
        assert_eq!(builder.accumulated_text.len(), 23); // 6 + 17 bytes
        assert_eq!(builder.accumulated_char_count, 14); // 6 + 8 chars
    }

    #[test]
    fn test_layout_hints_for_heading() {
        // Headings should emit layout_hints: [treat_as_title]
        let mut chapter = Chapter::new();
        let text_range = chapter.append_text("Chapter 1");
        let mut text_node = Node::new(Role::Text);
        text_node.text = text_range;
        let text_id = chapter.alloc_node(text_node);

        let heading = Node::new(Role::Heading(1));
        let heading_id = chapter.alloc_node(heading);
        chapter.append_child(heading_id, text_id);
        chapter.append_child(chapter.root(), heading_id);

        let mut ctx = crate::formats::kfx::context::ExportContext::new();
        ctx.register_section("test_section");
        let ion = build_storyline_ion(&chapter, &mut ctx);

        // Find layout_hints in the generated Ion
        fn find_layout_hints(ion: &IonValue) -> Option<Vec<u64>> {
            match ion {
                IonValue::Struct(fields) => {
                    for (key, value) in fields {
                        if *key == sym!(LayoutHints)
                            && let IonValue::List(items) = value
                        {
                            return Some(
                                items
                                    .iter()
                                    .filter_map(|v| {
                                        if let IonValue::Symbol(s) = v {
                                            Some(*s)
                                        } else {
                                            None
                                        }
                                    })
                                    .collect(),
                            );
                        }
                        if let Some(hints) = find_layout_hints(value) {
                            return Some(hints);
                        }
                    }
                    None
                }
                IonValue::List(items) => {
                    for item in items {
                        if let Some(hints) = find_layout_hints(item) {
                            return Some(hints);
                        }
                    }
                    None
                }
                _ => None,
            }
        }

        let hints = find_layout_hints(&ion);
        assert!(hints.is_some(), "Heading should have layout_hints");
        let hints = hints.unwrap();
        assert!(
            hints.contains(&(KfxSymbol::TreatAsTitle as u64)),
            "Heading layout_hints should contain treat_as_title"
        );
    }

    #[test]
    fn test_layout_hints_for_figure() {
        // Figure elements should emit layout_hints: [figure]
        let mut chapter = Chapter::new();

        let figure = Node::new(Role::Figure);
        let figure_id = chapter.alloc_node(figure);
        chapter.append_child(chapter.root(), figure_id);

        let mut ctx = crate::formats::kfx::context::ExportContext::new();
        ctx.register_section("test_section");
        let ion = build_storyline_ion(&chapter, &mut ctx);

        // Find layout_hints in the generated Ion
        fn find_layout_hints(ion: &IonValue) -> Option<Vec<u64>> {
            match ion {
                IonValue::Struct(fields) => {
                    for (key, value) in fields {
                        if *key == sym!(LayoutHints)
                            && let IonValue::List(items) = value
                        {
                            return Some(
                                items
                                    .iter()
                                    .filter_map(|v| {
                                        if let IonValue::Symbol(s) = v {
                                            Some(*s)
                                        } else {
                                            None
                                        }
                                    })
                                    .collect(),
                            );
                        }
                        if let Some(hints) = find_layout_hints(value) {
                            return Some(hints);
                        }
                    }
                    None
                }
                IonValue::List(items) => {
                    for item in items {
                        if let Some(hints) = find_layout_hints(item) {
                            return Some(hints);
                        }
                    }
                    None
                }
                _ => None,
            }
        }

        let hints = find_layout_hints(&ion);
        assert!(hints.is_some(), "Figure should have layout_hints");
        let hints = hints.unwrap();
        assert!(
            hints.contains(&(KfxSymbol::Figure as u64)),
            "Figure layout_hints should contain figure"
        );
    }

    #[test]
    fn test_yj_classification_for_footnote_popup() {
        // A note's body carries yj.classification ($615), which is what makes
        // a note body a footnote popup.
        let mut chapter = Chapter::new();

        // Create a list item that represents an endnote
        let text_range = chapter.append_text("This is footnote content");
        let mut text_node = Node::new(Role::Text);
        text_node.text = text_range;
        let text_id = chapter.alloc_node(text_node);

        let endnote = Node::new(Role::ListItem);
        let endnote_id = chapter.alloc_node(endnote);
        chapter.append_child(endnote_id, text_id);
        chapter.append_child(chapter.root(), endnote_id);

        // Set epub:type to indicate this is an endnote
        chapter
            .semantics
            .set_epub_type(endnote_id, "endnote footnote");
        chapter.semantics.set_id(endnote_id, "note-1");

        let mut ctx = crate::formats::kfx::context::ExportContext::new();
        ctx.register_section("test_section");
        let ion = build_storyline_ion(&chapter, &mut ctx);

        // Find yj.classification in the generated Ion and check its value
        fn find_classification(ion: &IonValue) -> Option<u64> {
            match ion {
                IonValue::Struct(fields) => {
                    for (key, value) in fields {
                        if *key == sym!(YjClassification)
                            && let IonValue::Symbol(sym) = value
                        {
                            return Some(*sym);
                        }
                        if let Some(found) = find_classification(value) {
                            return Some(found);
                        }
                    }
                    None
                }
                IonValue::List(items) => {
                    for item in items {
                        if let Some(found) = find_classification(item) {
                            return Some(found);
                        }
                    }
                    None
                }
                _ => None,
            }
        }

        let classification = find_classification(&ion);
        assert!(
            classification.is_some(),
            "Endnote element should have yj.classification attribute"
        );
        assert_eq!(
            classification.unwrap(),
            KfxSymbol::YjChapternote as u64,
            "an endnote is a note at the end of its section"
        );
    }

    #[test]
    fn each_kind_of_note_keeps_its_own_classification() {
        // Books use all three side by side, and neither direction may collapse
        // them into one.
        for (epub_type, classification) in [
            ("footnote", KfxSymbol::Footnote),
            ("endnote", KfxSymbol::YjChapternote),
            ("rearnote", KfxSymbol::YjEndnote),
            ("sidebar", KfxSymbol::YjSidenote),
        ] {
            assert_eq!(
                note_classification(epub_type),
                Some(classification as u64),
                "{epub_type} exports as its own classification"
            );
            assert_eq!(
                note_epub_type(classification as u64),
                Some(epub_type),
                "{epub_type} comes back from its own classification"
            );
        }
        assert_eq!(note_classification("bodymatter chapter"), None);
        assert_eq!(note_epub_type(KfxSymbol::Text as u64), None);
    }

    #[test]
    fn a_note_body_arrives_as_its_epub_type() {
        let symbols = no_symbols();
        let storyline = IonValue::Struct(vec![(
            sym!(ContentList),
            IonValue::List(vec![IonValue::Struct(vec![
                (sym!(Type), IonValue::Symbol(KfxSymbol::Text as u64)),
                (
                    sym!(YjClassification),
                    IonValue::Symbol(KfxSymbol::YjChapternote as u64),
                ),
            ])]),
        )]);

        let stream = tokenize_storyline(&storyline, &symbols, None, None, None);
        let Some(KfxToken::StartElement(elem)) = stream.iter().next() else {
            panic!("expected an element");
        };
        assert_eq!(elem.get_semantic(SemanticTarget::EpubType), Some("endnote"));
    }

    #[test]
    fn a_list_that_resumes_counting_stays_ordered() {
        // A numbered list broken by prose arrives as one list per item, each
        // stating where it resumes. An offset-less fragment restarts at one,
        // and a `numeric` list on its own carries no order at all.
        let symbols = no_symbols();
        let storyline = IonValue::Struct(vec![(
            sym!(ContentList),
            IonValue::List(vec![IonValue::Struct(vec![
                (sym!(Type), IonValue::Symbol(KfxSymbol::List as u64)),
                (sym!(ListStyle), IonValue::Symbol(KfxSymbol::Numeric as u64)),
                (sym!(ListStartOffset), IonValue::Int(7)),
            ])]),
        )]);

        let stream = tokenize_storyline(&storyline, &symbols, None, None, None);
        let Some(KfxToken::StartElement(elem)) = stream.iter().next() else {
            panic!("expected an element");
        };
        assert_eq!(elem.role, Role::OrderedList);
        assert_eq!(elem.list_start, Some(7));
    }

    #[test]
    fn test_heading_with_border_exports_as_container() {
        // Test that elements with borders are wrapped in type: container
        // with nested type: text for KFX border rendering
        use crate::style::{BorderStyle, ComputedStyle, Length};

        let mut chapter = Chapter::new();

        // Create a heading with border style
        let mut style = ComputedStyle::default();
        style.border_style_top = BorderStyle::Solid;
        style.border_width_top = Length::Px(2.0);
        let style_id = chapter.styles.intern(style);

        let mut h1 = Node::new(Role::Heading(1));
        h1.style = style_id;
        let h1_id = chapter.alloc_node(h1);
        chapter.append_child(chapter.root(), h1_id);

        // Add text content
        let text_range = chapter.append_text("Title with Border");
        let mut text_node = Node::new(Role::Text);
        text_node.text = text_range;
        let text_id = chapter.alloc_node(text_node);
        chapter.append_child(h1_id, text_id);

        let mut ctx = crate::formats::kfx::context::ExportContext::new();
        let ion = build_storyline_ion(&chapter, &mut ctx);

        // Helper to find element type and nested content_list structure
        fn find_container_structure(ion: &IonValue) -> Option<(u64, Option<u64>)> {
            match ion {
                IonValue::Struct(fields) => {
                    let mut elem_type = None;
                    let mut inner_type = None;

                    for (key, value) in fields {
                        if *key == KfxSymbol::Type as u64
                            && let IonValue::Symbol(sym) = value
                        {
                            elem_type = Some(*sym);
                        }
                        if *key == KfxSymbol::ContentList as u64
                            && let IonValue::List(items) = value
                        {
                            for item in items {
                                if let Some((inner_elem_type, _)) = find_container_structure(item) {
                                    inner_type = Some(inner_elem_type);
                                    break;
                                }
                            }
                        }
                    }

                    if let Some(t) = elem_type {
                        return Some((t, inner_type));
                    }
                    None
                }
                IonValue::List(items) => {
                    for item in items {
                        if let Some(result) = find_container_structure(item) {
                            return Some(result);
                        }
                    }
                    None
                }
                _ => None,
            }
        }

        let structure = find_container_structure(&ion);
        assert!(structure.is_some(), "Should find element structure");
        let (outer_type, inner_type) = structure.unwrap();

        // Outer element should be type: container
        assert_eq!(
            outer_type,
            KfxSymbol::Container as u64,
            "Heading with border should have type: container (not text)"
        );

        // Should have nested type: text child
        assert!(
            inner_type.is_some(),
            "Container should have nested content_list with inner element"
        );
        assert_eq!(
            inner_type.unwrap(),
            KfxSymbol::Text as u64,
            "Inner element should have type: text"
        );
    }

    #[test]
    fn test_horizontal_rule_keeps_its_element_type() {
        // An `<hr>` draws its line from a border, within reach of the
        // bordered-box path. It emits as the bare `{style: linear, type:
        // horizontal_rule}` this crate's importer reads back as `Role::Rule`.
        use crate::style::{BorderStyle, ComputedStyle, Length};

        let mut chapter = Chapter::new();

        let mut style = ComputedStyle::default();
        style.border_style_top = BorderStyle::Solid;
        style.border_width_top = Length::Px(1.0);
        let style_id = chapter.styles.intern(style);

        let mut rule = Node::new(Role::Rule);
        rule.style = style_id;
        let rule_id = chapter.alloc_node(rule);
        chapter.append_child(chapter.root(), rule_id);

        let mut ctx = crate::formats::kfx::context::ExportContext::new();
        let ion = build_storyline_ion(&chapter, &mut ctx);

        fn collect_types(ion: &IonValue, out: &mut Vec<u64>) {
            match ion {
                IonValue::Struct(fields) => {
                    for (k, v) in fields {
                        if *k == KfxSymbol::Type as u64
                            && let IonValue::Symbol(s) = v
                        {
                            out.push(*s);
                        }
                        collect_types(v, out);
                    }
                }
                IonValue::List(items) => items.iter().for_each(|i| collect_types(i, out)),
                _ => {}
            }
        }

        let mut types = Vec::new();
        collect_types(&ion, &mut types);
        assert_eq!(
            types,
            vec![KfxSymbol::HorizontalRule as u64],
            "a bordered <hr> must export as one horizontal_rule element, \
             not a container wrapping an empty text block"
        );
    }

    /// A section-break ornament is an `<hr>` whose whole appearance is a CSS
    /// background picture, with the rule itself switched off. Both halves have
    /// to reach the KFX style: the picture as a `background_image` symbol
    /// naming the resource, and the explicit `border_style: none` — a KFX
    /// `horizontal_rule` draws its line from the border properties (Amazon's
    /// own rule styles carry `border_style` + `border_weight`). A style
    /// that says nothing gets the device's default line drawn across it.
    #[test]
    fn ornament_hr_exports_its_background_and_suppressed_rule() {
        use crate::style::{BackgroundRepeat, BorderStyle, ComputedStyle};

        let mut chapter = Chapter::new();
        let mut style = ComputedStyle {
            background_image: Some("OEBPS/images/asterisks.jpg".to_string()),
            background_repeat: BackgroundRepeat::NoRepeat,
            ..Default::default()
        };
        style.border_style_top = BorderStyle::None;
        style.border_style_bottom = BorderStyle::None;
        let style_id = chapter.styles.intern(style);

        let mut rule = Node::new(Role::Rule);
        rule.style = style_id;
        let rule_id = chapter.alloc_node(rule);
        chapter.append_child(chapter.root(), rule_id);

        let mut ctx = crate::formats::kfx::context::ExportContext::new();
        // Export Pass 1 mints a short name and symbol for every media asset;
        // the style lookup is immutable and relies on that having happened.
        let short = ctx
            .resource_registry
            .get_or_create_name("OEBPS/images/asterisks.jpg");
        let expected = ctx.symbols.get_or_intern(&short);

        let _ = build_storyline_ion(&chapter, &mut ctx);

        let (_, style) = ctx
            .style_registry
            .styles()
            .find(|(_, s)| s.get(KfxSymbol::BackgroundImage).is_some())
            .expect("the ornament's style carries a background_image");
        assert_eq!(
            style.get(KfxSymbol::BackgroundImage),
            Some(&crate::formats::kfx::style_schema::KfxValue::SymbolId(
                expected
            )),
            "background_image must name the external_resource by symbol"
        );
        assert_eq!(
            style.get(KfxSymbol::BackgroundRepeat),
            Some(&crate::formats::kfx::style_schema::KfxValue::Symbol(
                KfxSymbol::NoRepeat
            ))
        );
        assert_eq!(
            style.get(KfxSymbol::BorderStyleTop),
            Some(&crate::formats::kfx::style_schema::KfxValue::Symbol(
                KfxSymbol::None
            )),
            "an <hr> told to draw no border must say so, or the device rules its own line"
        );
    }

    #[test]
    fn test_bordered_container_layout_follows_writing_mode() {
        // A bordered `type: container`'s `layout` is its children's
        // block-progression axis, keyed to the box's own writing mode:
        // `horizontal` for vertical text (縦書き), `vertical` for horizontal-tb.
        use crate::style::{BorderStyle, ComputedStyle, Length, WritingMode};

        fn container_layout_for(wm: WritingMode) -> Option<u64> {
            let mut chapter = Chapter::new();

            let mut style = ComputedStyle::default();
            style.border_style_top = BorderStyle::Solid;
            style.border_width_top = Length::Px(1.0);
            style.writing_mode = wm;
            let style_id = chapter.styles.intern(style);

            let mut boxed = Node::new(Role::Paragraph);
            boxed.style = style_id;
            let boxed_id = chapter.alloc_node(boxed);
            chapter.append_child(chapter.root(), boxed_id);

            let text_range = chapter.append_text("囲み");
            let mut text_node = Node::new(Role::Text);
            text_node.text = text_range;
            let text_id = chapter.alloc_node(text_node);
            chapter.append_child(boxed_id, text_id);

            let mut ctx = crate::formats::kfx::context::ExportContext::new();
            let ion = build_storyline_ion(&chapter, &mut ctx);

            fn find_container_layout(ion: &IonValue) -> Option<u64> {
                match ion {
                    IonValue::Struct(fields) => {
                        let is_container = fields.iter().any(|(k, v)| {
                            *k == KfxSymbol::Type as u64
                                && matches!(v, IonValue::Symbol(s) if *s == KfxSymbol::Container as u64)
                        });
                        if is_container {
                            for (k, v) in fields {
                                if *k == KfxSymbol::Layout as u64
                                    && let IonValue::Symbol(s) = v
                                {
                                    return Some(*s);
                                }
                            }
                        }
                        fields.iter().find_map(|(k, v)| {
                            (*k == KfxSymbol::ContentList as u64)
                                .then(|| find_container_layout(v))
                                .flatten()
                        })
                    }
                    IonValue::List(items) => items.iter().find_map(find_container_layout),
                    _ => None,
                }
            }

            find_container_layout(&ion)
        }

        assert_eq!(
            container_layout_for(WritingMode::VerticalRl),
            Some(KfxSymbol::Horizontal as u64),
            "vertical-rl (縦書き) box container must be layout: horizontal"
        );
        assert_eq!(
            container_layout_for(WritingMode::VerticalLr),
            Some(KfxSymbol::Horizontal as u64),
            "vertical-lr box container must be layout: horizontal"
        );
        assert_eq!(
            container_layout_for(WritingMode::HorizontalTb),
            Some(KfxSymbol::Vertical as u64),
            "horizontal-tb box container must be layout: vertical"
        );
    }

    #[test]
    fn test_heading_without_border_exports_as_text() {
        // Test that elements without borders use normal type: text
        let mut chapter = Chapter::new();

        // Create a heading without border style
        let h1 = Node::new(Role::Heading(1));
        let h1_id = chapter.alloc_node(h1);
        chapter.append_child(chapter.root(), h1_id);

        // Add text content
        let text_range = chapter.append_text("Title without Border");
        let mut text_node = Node::new(Role::Text);
        text_node.text = text_range;
        let text_id = chapter.alloc_node(text_node);
        chapter.append_child(h1_id, text_id);

        let mut ctx = crate::formats::kfx::context::ExportContext::new();
        let ion = build_storyline_ion(&chapter, &mut ctx);

        // Helper to find first element type
        fn find_first_element_type(ion: &IonValue) -> Option<u64> {
            match ion {
                IonValue::Struct(fields) => {
                    for (key, value) in fields {
                        if *key == KfxSymbol::Type as u64
                            && let IonValue::Symbol(sym) = value
                        {
                            return Some(*sym);
                        }
                    }
                    None
                }
                IonValue::List(items) => {
                    for item in items {
                        if let Some(result) = find_first_element_type(item) {
                            return Some(result);
                        }
                    }
                    None
                }
                _ => None,
            }
        }

        let elem_type = find_first_element_type(&ion);
        assert!(elem_type.is_some(), "Should find element type");

        // Element without border should be type: text (normal heading)
        assert_eq!(
            elem_type.unwrap(),
            KfxSymbol::Text as u64,
            "Heading without border should have type: text"
        );
    }

    #[test]
    fn test_needs_container_wrapper_no_border() {
        let style = ComputedStyle::default();
        assert!(!needs_container_wrapper(&style));
    }

    #[test]
    fn test_needs_container_wrapper_with_top_border() {
        let mut style = ComputedStyle::default();
        style.border_style_top = BorderStyle::Solid;
        style.border_width_top = Length::Px(1.0);
        assert!(needs_container_wrapper(&style));
    }

    #[test]
    fn test_needs_container_wrapper_with_bottom_border() {
        let mut style = ComputedStyle::default();
        style.border_style_bottom = BorderStyle::Solid;
        style.border_width_bottom = Length::Px(2.0);
        assert!(needs_container_wrapper(&style));
    }

    #[test]
    fn test_needs_container_wrapper_border_style_none() {
        let mut style = ComputedStyle::default();
        // Has width but no style - should NOT need wrapper
        style.border_style_top = BorderStyle::None;
        style.border_width_top = Length::Px(1.0);
        assert!(!needs_container_wrapper(&style));
    }

    #[test]
    fn test_needs_container_wrapper_border_width_zero() {
        let mut style = ComputedStyle::default();
        // Has style but zero width - should NOT need wrapper
        style.border_style_top = BorderStyle::Solid;
        style.border_width_top = Length::Px(0.0);
        assert!(!needs_container_wrapper(&style));
    }

    #[test]
    fn test_nested_spans_link_containing_inline() {
        // Test that nested spans (Link containing Inline) are properly reconstructed.
        // This is the TOC case: "1. Easy Concurrency..." where "1." is in an Inline inside a Link.
        let mut stream = TokenStream::new();
        let mut link_semantics = HashMap::new();
        link_semantics.insert(SemanticTarget::Href, "#chapter1".to_string());

        // Text: "1. Easy Concurrency"
        // Link: offset 0, length 19 (entire text)
        // Inline: offset 0, length 2 ("1.")
        stream.push(KfxToken::StartElement(ElementStart {
            role: Role::Paragraph,
            node_id: None,
            id: None,
            semantics: HashMap::new(),
            content_ref: Some(ContentRef {
                name: "content_1".to_string(),
                index: 0,
            }),
            style_events: vec![
                SpanStart {
                    role: Role::Link,
                    node_id: None,
                    semantics: link_semantics,
                    offset: 0,
                    length: 19,
                    style_symbol: None,
                    kfx_attrs: Vec::new(),
                    ruby_annotation: None,
                    ruby_pairs: Vec::new(),
                },
                SpanStart {
                    role: Role::Inline,
                    node_id: None,
                    semantics: HashMap::new(),
                    offset: 0,
                    length: 2,
                    style_symbol: None,
                    kfx_attrs: Vec::new(),
                    ruby_annotation: None,
                    ruby_pairs: Vec::new(),
                },
            ],
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
        stream.end_element();

        let chapter = build_ir_from_tokens(&stream, &no_symbols(), None, |_, _| {
            Some("1. Easy Concurrency".to_string())
        });

        // Paragraph → Link [href="#chapter1"] → Inline → Text "1.", then
        // Text " Easy Concurrency" as the Link's second child.
        let para_id = chapter.children(chapter.root()).next().unwrap();
        let para_children: Vec<_> = chapter.children(para_id).collect();

        // Should have exactly one child: the Link
        assert_eq!(
            para_children.len(),
            1,
            "Paragraph should have one Link child"
        );

        let link_id = para_children[0];
        let link_node = chapter.node(link_id).unwrap();
        assert_eq!(link_node.role, Role::Link, "First child should be Link");
        assert_eq!(
            chapter.semantics.href(link_id),
            Some("#chapter1"),
            "Link should have href"
        );

        // Link should have two children: Inline and Text
        let link_children: Vec<_> = chapter.children(link_id).collect();
        assert_eq!(
            link_children.len(),
            2,
            "Link should have Inline + Text children"
        );

        // First child: Inline containing "1."
        let inline_id = link_children[0];
        let inline_node = chapter.node(inline_id).unwrap();
        assert_eq!(
            inline_node.role,
            Role::Inline,
            "First Link child should be Inline"
        );

        let inline_children: Vec<_> = chapter.children(inline_id).collect();
        assert_eq!(
            inline_children.len(),
            1,
            "Inline should have one Text child"
        );
        let inline_text = chapter.node(inline_children[0]).unwrap();
        assert_eq!(chapter.text(inline_text.text), "1.");

        // Second child: Text " Easy Concurrency"
        let text_id = link_children[1];
        let text_node = chapter.node(text_id).unwrap();
        assert_eq!(text_node.role, Role::Text);
        assert_eq!(chapter.text(text_node.text), " Easy Concurrency");
    }

    #[test]
    fn test_flatten_inline_content_produces_non_overlapping_segments() {
        // Test the "Push Down, Emit at Bottom" flattening algorithm.
        // Given: Link > Inline > Text("1.") + Text("Easy...")
        // Expect: Two non-overlapping segments, each with correct accumulated state.

        let mut chapter = Chapter::new();

        // Create distinct styles (use different margin values to distinguish)
        let link_style = chapter.styles.intern(ComputedStyle::default());
        let mut inline_computed = ComputedStyle::default();
        inline_computed.margin_top = Length::Px(10.0);
        let inline_style = chapter.styles.intern(inline_computed);

        // Build tree: Link > Inline > Text("1.") + Text(" Easy")
        // Create text nodes
        let text1_range = chapter.append_text("1.");
        let mut text1 = Node::new(Role::Text);
        text1.text = text1_range;
        let text1_id = chapter.alloc_node(text1);

        let text2_range = chapter.append_text(" Easy Concurrency");
        let mut text2 = Node::new(Role::Text);
        text2.text = text2_range;
        let text2_id = chapter.alloc_node(text2);

        // Create Inline containing text1
        let mut inline_node = Node::new(Role::Inline);
        inline_node.style = inline_style;
        let inline_id = chapter.alloc_node(inline_node);
        chapter.append_child(inline_id, text1_id);

        // Create Link containing Inline and text2
        let mut link_node = Node::new(Role::Link);
        link_node.style = link_style;
        let link_id = chapter.alloc_node(link_node);
        chapter.append_child(link_id, inline_id);
        chapter.append_child(link_id, text2_id);
        chapter.semantics.set_href(link_id, "#chapter1");

        // Flatten the Link subtree
        let mut segments = Vec::new();
        flatten_inline_content(&chapter, link_id, InlineState::default(), &mut segments);

        // Should produce exactly 2 segments
        assert_eq!(segments.len(), 2, "Should have 2 non-overlapping segments");

        // First segment: "1." with Inline's style and Link's href
        let FlatSegment::Text { text, state } = &segments[0] else {
            panic!("First segment should be Text, got Image");
        };
        assert_eq!(text, "1.");
        assert_eq!(
            state.link_to,
            Some("#chapter1".to_string()),
            "First segment should have link_to from Link"
        );
        assert_eq!(
            state.style,
            Some(inline_style),
            "First segment should have Inline's style (innermost wins)"
        );

        // Second segment: " Easy Concurrency" with Link's style and Link's href
        let FlatSegment::Text { text, state } = &segments[1] else {
            panic!("Second segment should be Text, got Image");
        };
        assert_eq!(text, " Easy Concurrency");
        assert_eq!(
            state.link_to,
            Some("#chapter1".to_string()),
            "Second segment should have link_to from Link"
        );
        assert_eq!(
            state.style,
            Some(link_style),
            "Second segment should have Link's style"
        );
    }

    #[test]
    fn test_flatten_linked_image_with_id_emits_anchor_carrier() {
        // `<a id="map1"><img/></a>` as a TOC/nav target. A KFX image element
        // holds no anchor: the flattener emits a zero-width-space segment
        // carrying the id and node_id just before the image.
        let mut chapter = Chapter::new();

        let img_id = chapter.alloc_node(Node::new(Role::Image));
        chapter.semantics.set_src(img_id, "images/map1.jpg");

        let link_id = chapter.alloc_node(Node::new(Role::Link));
        chapter.append_child(link_id, img_id);
        chapter.semantics.set_id(link_id, "map1");
        chapter.semantics.set_href(link_id, "Contents.xhtml#rmap1");

        let mut segments = Vec::new();
        flatten_inline_content(&chapter, link_id, InlineState::default(), &mut segments);

        assert_eq!(segments.len(), 2, "expected anchor carrier + image");
        let FlatSegment::Text { text, state } = &segments[0] else {
            panic!("first segment should be the ZWSP anchor carrier, got an image");
        };
        assert_eq!(text, "\u{200B}");
        assert_eq!(state.element_id, Some("map1".to_string()));
        assert_eq!(
            state.node_id,
            Some(link_id),
            "carrier must reference the id-bearing <a> node so its position is recorded"
        );
        assert!(
            matches!(segments[1], FlatSegment::Image { node_id } if node_id == img_id),
            "second segment should be the image itself"
        );
    }

    #[test]
    fn test_flatten_plain_linked_image_emits_no_anchor_carrier() {
        // Counterpart: an `<a href><img/></a>` with NO id is not an anchor
        // target, and no zero-width carrier is emitted — just the image.
        let mut chapter = Chapter::new();

        let img_id = chapter.alloc_node(Node::new(Role::Image));
        chapter.semantics.set_src(img_id, "images/plain.jpg");

        let link_id = chapter.alloc_node(Node::new(Role::Link));
        chapter.append_child(link_id, img_id);
        chapter.semantics.set_href(link_id, "https://example.com");

        let mut segments = Vec::new();
        flatten_inline_content(&chapter, link_id, InlineState::default(), &mut segments);

        assert_eq!(segments.len(), 1, "no id → no anchor carrier");
        assert!(matches!(segments[0], FlatSegment::Image { node_id } if node_id == img_id));
    }

    #[test]
    fn test_anchor_inside_container_wrapper_uses_outer_id() {
        // Test that anchors inside container-wrapped elements (like headings with borders)
        // use the outer container's ID, not the inner text element's ID.
        // This is critical for TOC navigation to work correctly.
        use crate::model::ChapterId;
        use crate::style::{BorderStyle, ComputedStyle, Length};

        let mut chapter = Chapter::new();

        // Create a heading with border style (triggers container wrapper)
        let mut style = ComputedStyle::default();
        style.border_style_bottom = BorderStyle::Solid;
        style.border_width_bottom = Length::Px(1.0);
        let style_id = chapter.styles.intern(style);

        let mut h2 = Node::new(Role::Heading(2));
        h2.style = style_id;
        let h2_id = chapter.alloc_node(h2);
        chapter.append_child(chapter.root(), h2_id);

        // Add text content
        let text_range = chapter.append_text("All the Tools You Need");
        let mut text_node = Node::new(Role::Text);
        text_node.text = text_range;
        let text_id = chapter.alloc_node(text_node);
        chapter.append_child(h2_id, text_id);

        // Add an inline span with an ID (like <span id="p6"/>)
        // This simulates how EPUB anchors are often placed
        let span_node = Node::new(Role::Inline);
        let span_id = chapter.alloc_node(span_node);
        chapter.append_child(h2_id, span_id);
        chapter.semantics.set_id(span_id, "p6");

        let mut ctx = crate::formats::kfx::context::ExportContext::new();

        // Set up the context with a chapter ID
        let chapter_id = ChapterId(1);
        ctx.begin_chapter_export(chapter_id);

        // Register the span as a link target (simulating what resolve_links does)
        let target = GlobalNodeId::new(chapter_id, span_id);
        ctx.anchor_registry
            .register_internal_target(target, "chapter1.xhtml#p6");

        let _ion = build_storyline_ion(&chapter, &mut ctx);

        // Get the node position for p6
        let anchor_pos = ctx.anchor_registry.get_node_position(target);

        // The anchor position should exist and point to the outer container ID
        assert!(anchor_pos.is_some(), "Anchor for p6 should be created");

        let (fragment_id, _offset) = anchor_pos.unwrap();

        // Get the list of content IDs recorded for this chapter
        // Container wrapper creates 2 content IDs: outer container and inner text
        // The first one (outer container) should be used for the anchor
        let content_ids = ctx.content_ids_by_chapter.get(&chapter_id);
        assert!(
            content_ids.is_some(),
            "Should have recorded content IDs for chapter"
        );
        let content_ids = content_ids.unwrap();
        assert!(
            content_ids.len() >= 2,
            "Container wrapper should create at least 2 content IDs (outer + inner), got {}",
            content_ids.len()
        );

        // The anchor should point to the first content ID (the outer container)
        // not the second ID (the inner text element)
        assert_eq!(
            fragment_id, content_ids[0],
            "Anchor should point to outer container ID ({}) not inner element ({})",
            content_ids[0], content_ids[1]
        );
    }
}
