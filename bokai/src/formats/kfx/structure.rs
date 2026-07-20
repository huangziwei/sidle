//! Queries over a loaded [`BookData`].
//!
//! Small readers for facts that live in the fragment graph rather than in any
//! one entity, usable without standing up a whole conversion pipeline.

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

/// Resolve `$145` text: either a literal string or a struct
/// `{name, $403: index}` pointing at a `book_data["$145"][name].$146[i]`.
pub fn resolve_content_text(value: &IonValue, book: &BookData) -> String {
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
