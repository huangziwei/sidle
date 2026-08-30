//! Queries over a book's fragment graph.

use std::collections::HashMap;

use crate::formats::kfx::container::get_field;
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::loader::BookData;
use crate::formats::kfx::symbols::KfxSymbol;

/// One fragment of the given type, by name. Returns a reference into `book`, so
/// a caller walking a storyline never clones the tree it is reading.
pub fn lookup_fragment<'b>(
    book: &'b BookData,
    ftype: KfxSymbol,
    name: &str,
) -> Option<&'b IonValue> {
    book.by_type.get(&(ftype as u64)).and_then(|m| m.get(name))
}

/// The book's reading orders, each as its list of section names.
pub fn reading_orders(book: &BookData) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    for type_id in [KfxSymbol::DocumentData as u64, KfxSymbol::Metadata as u64] {
        let Some(map) = book.by_type.get(&type_id) else {
            continue;
        };
        for value in map.values() {
            let Some(fields) = value.unwrap_annotated().as_struct() else {
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
                let names: Vec<String> = sections
                    .iter()
                    .filter_map(|s| book.symbols.text_of(s).map(|n| n.to_string()))
                    .collect();
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

/// Every `$155 id` reachable from a page_template, in walk order.
pub fn collect_element_ids(template: &IonValue, book: &BookData, out: &mut Vec<i64>) {
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    walk_element_ids(template, book, &mut visited, out);
}

fn walk_element_ids(
    value: &IonValue,
    book: &BookData,
    visited: &mut std::collections::HashSet<String>,
    out: &mut Vec<i64>,
) {
    let inner = value.unwrap_annotated();
    match inner {
        IonValue::Struct(fields) => {
            if let Some(id) = get_field(fields, KfxSymbol::Id as u64).and_then(|v| v.as_int()) {
                out.push(id);
            }
            if let Some(story) = get_field(fields, KfxSymbol::StoryName as u64)
                && let Some(name) = book.symbols.text_of(story)
                && visited.insert(name.to_string())
                && let Some(storyline) = lookup_fragment(book, KfxSymbol::Storyline, name)
            {
                walk_element_ids(storyline, book, visited, out);
            }
            for (_, v) in fields {
                walk_element_ids(v, book, visited, out);
            }
        }
        IonValue::List(items) => {
            for item in items {
                walk_element_ids(item, book, visited, out);
            }
        }
        _ => {}
    }
}

/// Layout hints + heading level from a named `$157 style` entity, resolved out
/// of `book.by_type[$157]` (see
/// [`style_fields_layout_hints`](super::yj_properties::style_fields_layout_hints)
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
pub fn eid_text_map(book: &BookData) -> HashMap<i64, String> {
    let mut out = HashMap::new();
    if let Some(storylines) = book.by_type.get(&(KfxSymbol::Storyline as u64)) {
        for story in storylines.values() {
            collect_eid_text(story, book, &mut out);
        }
    }
    out
}
