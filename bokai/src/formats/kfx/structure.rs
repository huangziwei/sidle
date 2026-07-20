//! Queries over a book's fragment graph.
//!
//! Small readers for facts that live across fragments rather than in any one
//! entity, usable without standing up a whole conversion pipeline. The text
//! walks take a [`ContentSource`] so they work either against a fully loaded
//! [`BookData`] or against a reader that parses one entity at a time.

use std::collections::HashMap;

use crate::formats::kfx::container::get_field;
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::loader::BookData;
use crate::formats::kfx::symbols::KfxSymbol;

/// Layout hints + heading level from a named `$157 style` entity, resolved out
/// of `book.by_type[$157]` (see
/// [`style_fields_layout_hints`](super::yj_properties::style_fields_layout_hints)
/// for the field semantics). Empty when the name doesn't resolve.
pub fn style_layout_hints_for(style_name: &str, book: &BookData) -> (Vec<String>, Option<String>) {
    let Some(styles) = book.by_type.get(&(KfxSymbol::Style as u64)) else {
        return (Vec::new(), None);
    };
    let Some(value) = styles.get(style_name) else {
        return (Vec::new(), None);
    };
    let Some(fields) = value.unwrap_annotated().as_struct() else {
        return (Vec::new(), None);
    };
    super::yj_properties::style_fields_layout_hints(fields, &book.symbols)
}

/// How a `$145 content` reference reaches the string it names.
///
/// A reference takes the form `{name, $403: index}` and points at entry
/// `index` of content entity `name`. Callers differ in how they reach that
/// entity — a loaded [`BookData`] holds every one in memory, while a reader
/// working off the container's entity index parses one on demand — so the
/// lookup is supplied rather than assumed.
pub trait ContentSource {
    /// The symbol table the reference's `name` field resolves through.
    fn symbols(&self) -> &crate::formats::kfx::container::SymbolTable;
    /// Entry `index` of the named content entity, if both resolve.
    fn content_string(&self, name: &str, index: usize) -> Option<String>;
}

impl ContentSource for BookData {
    fn symbols(&self) -> &crate::formats::kfx::container::SymbolTable {
        &self.symbols
    }

    fn content_string(&self, name: &str, index: usize) -> Option<String> {
        let entry = self.by_type.get(&(KfxSymbol::Content as u64))?.get(name)?;
        let list = entry
            .unwrap_annotated()
            .as_struct()
            .and_then(|fs| get_field(fs, KfxSymbol::ContentList as u64))
            .and_then(|v| v.as_list())?;
        list.get(index)?.as_string().map(|s| s.to_string())
    }
}

/// Resolve `$145` text: either a literal string or a reference resolved
/// through `source`.
pub fn resolve_content_text_from(value: &IonValue, source: &impl ContentSource) -> String {
    let inner = value.unwrap_annotated();
    if let Some(s) = inner.as_string() {
        return s.to_string();
    }
    if let Some(fields) = inner.as_struct() {
        let name = get_field(fields, KfxSymbol::Name as u64)
            .and_then(|v| source.symbols().text_of(v))
            .map(|s| s.to_string())
            .unwrap_or_default();
        let index = get_field(fields, KfxSymbol::Index as u64)
            .and_then(|v| v.as_int())
            .unwrap_or(0) as usize;
        if !name.is_empty()
            && let Some(s) = source.content_string(&name, index)
        {
            return s;
        }
    }
    String::new()
}

/// Resolve `$145` text against a loaded book.
pub fn resolve_content_text(value: &IonValue, book: &BookData) -> String {
    resolve_content_text_from(value, book)
}

/// Recursively walk a storyline fragment. Every struct that carries a
/// `$155 id` and a `$145 content` reference contributes `eid → base text`;
/// `$146 content_list` children are recursed into. `$176 story_name`
/// references are *not* followed — each referenced story is its own
/// `$259 storyline` fragment and is walked at the top level instead.
pub fn collect_eid_text(
    value: &IonValue,
    source: &impl ContentSource,
    out: &mut HashMap<i64, String>,
) {
    let inner = value.unwrap_annotated();
    if let Some(fields) = inner.as_struct() {
        if let Some(eid) = get_field(fields, KfxSymbol::Id as u64).and_then(|v| v.as_int())
            && let Some(content) = get_field(fields, KfxSymbol::Content as u64)
        {
            let text = resolve_content_text_from(content, source);
            if !text.is_empty() {
                out.insert(eid, text);
            }
        }
        if let Some(list) =
            get_field(fields, KfxSymbol::ContentList as u64).and_then(|v| v.as_list())
        {
            for child in list {
                collect_eid_text(child, source, out);
            }
        }
    } else if let Some(list) = inner.as_list() {
        for item in list {
            collect_eid_text(item, source, out);
        }
    }
}

/// The book's whole `eid → base text` map, walked from every storyline.
///
/// "Base text" is the element's own `$145 content`, with nothing added: this is
/// the substrate device anchors address, so a `(eid, offset)` pair indexes into
/// exactly this string.
pub fn eid_text_map(book: &BookData) -> HashMap<i64, String> {
    let mut out = HashMap::new();
    if let Some(storylines) = book.by_type.get(&(KfxSymbol::Storyline as u64)) {
        for story in storylines.values() {
            collect_eid_text(story, book, &mut out);
        }
    }
    out
}
